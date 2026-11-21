# Acceptance

The recorded QEMU runs and host assets are historical. Current win3d labs
use the in-tree vmlab-hv bundle and its patched Venus render server.
See [host.md](host.md) and [release.md](release.md) for current setup.

This page checks the "Done" criteria in `docs/brief.md` one by one, on fresh
vmlab labs from the local `windows-11` and `windows-10` templates. One script
produces all of the evidence, and anyone can run it again:

```sh
tests/e2e/run.sh --template windows-11     # or windows-10; --keep leaves the lab up
tests/e2e/run.sh --template windows-11 --host-gpu Nvidia1   # another host GPU (default AMD1)
tests/e2e/run.sh --template windows-11 --variant win3d      # a clone of #win3d: checks its install instead of installing
```

It exits 0 only when every Done check passes, and it prints a pass/fail
table. The tables below come from each run's `results.json`
(`tests/e2e/report.py`).

## What the script does

From nothing, with no manual step:

1. **Build.** Runs `scripts/build.sh`. If `out/package/` was built from a
   commit whose `build/`, `installer/`, `probes/`, `patches/` and
   `scripts/build.sh` match HEAD, it reuses that bundle instead, and
   `build.txt` records which of the two happened.
2. **Lab.** Copies `labs/acceptance/vmlab.wcl` into
   `.scratch/e2e/<template>/lab/`, sets the template and lab name
   (`win3d-e2e-<template>`), and runs `vmlab up`. `--host-gpu <name>`
   replaces `AMD1` with another GPU from the host config and adds
   `-<name in lower case>` to the scratch and evidence directories and the
   lab name; `--lab` names the lab outright. The lab has the
   `labs/probe/vmlab.wcl` shape for vmlab 0.25: `secure_boot = false` and
   `gpu { mode = "vulkan" host_gpu = "AMD1" }`, the AMD iGPU as the host
   config names it (`docs/host.md`). Without `host_gpu`, the lab would take
   the host's default GPU, which on helios is an RTX 5090. `vmlab up`
   replaces the templates' guest agent with the installed vmlab's. Before
   vmlab 0.31.0-alpha the lab set `agent_update = false`, because a late
   agent restart dropped the agent channel in one windows-10 run; 0.31 holds
   the channel while the agent is replaced.
   `qemu-argv-env.txt` keeps the QEMU command line and environment that vmlab
   logged, and, on vmlab 0.27, each distinct `vmlab-pci-filter:` line with
   its count. `status-v.txt` and `status-v-end.txt` keep the host GPU vmlab
   picked (`host_gpu=AMD1`) and, for a GPU with `qemu_binary`, `qemu=<path>`.
3. **Baseline.** Copies `tests/guest/`, `tests/e2e/guest/` and `out/probes/`
   into the guest and records `tests/guest/Get-GpuState.ps1`
   (`baseline-gpu-state.json`). It then creates the autologon test user with
   `tests/guest/Set-TestDesktop.ps1 -Reboot`, which first waits for
   Windows' user OOBE to finish (`test-desktop.txt`, `oobe:` line; see
   "Findings").
4. **Install.** Runs the documented command from `docs/install.md`,
   `scripts/install-into-vm.sh <lab-dir> <vm>`, unchanged (`install.log`,
   `install-journal.log`, `installed-gpu-state.json`). The script then checks
   that autologon survived the install's restarts (`autologon-*.txt`). If it
   did not, the script logs `autologon fallback fired`, saves the OOBE lines
   of `setupact.log` (`autologon-fallback.txt`) and runs
   `Set-TestDesktop.ps1 -Reboot` again.
5. **Session tests.** These run as the logged-on user in session 1, through
   `tests/guest/Run-Probe.ps1`. First the script presses Esc to close the
   Start menu, then it runs:
   - `vulkaninfo --summary` from System32 and SysWOW64, the loader's own
     tool installed by the package (`tests/e2e/guest/Invoke-Vulkaninfo.ps1`).
     LunarG's vulkaninfo 1.4.363, pinned in `tests/e2e/PINS` and run without
     its own loader, is an extra cross-check of the installed loader and ICD.
   - The D3D9, D3D11, D3D12, OpenGL and Vulkan probes (`probes/README.md`),
     x64 and x86, for 20 s each. Run-Probe samples each process's loaded
     modules (`probes/<arch>-<api>_probe.modules.txt`). While a probe runs,
     `vmlab vm screenshot` captures the console every 2 s.
     `tests/e2e/findprobe.py` looks in each capture for the probe's three
     marker squares. The capture from the middle of the run is kept as
     `console-<arch>-<api>_probe.png`, at half size.
   - `tests/guest/Get-Dxdiag.ps1 -AsUser` (`dxdiag/dxdiag.json`).
6. **Collect.** Copies the results out of the guest into
   [docs/evidence/acceptance/<template>/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/acceptance/), and `tests/e2e/check.py` judges
   them (`results.json`, `results.txt`). With `--apps`, the stretch
   applications run after this, so one that hangs the guest cannot cost
   the Done evidence.
   The adapter suite (`tests/e2e/adapter.sh`, see "Adapter suite") runs
   after them, last.
7. **Teardown.** Records the `qemu.log` lines and `vmlab status -v` again,
   then runs `vmlab destroy`, unless `--keep` was given.

### How each Done item is judged

| Done item (`docs/brief.md`) | Check (`tests/e2e/check.py`) |
| --- | --- |
| 1. One script builds the whole package | `scripts/build.sh` exits 0, or the bundle it built matches HEAD's sources |
| 2. One documented install command | `scripts/install-into-vm.sh` exits 0 and the guest's `state.json` says `Installed` |
| 2. `vulkaninfo` lists the Venus device | The installed x64 `vulkaninfo --summary` names a `Virtio-GPU Venus` device that is not a CPU device |
| 2. D3D9, D3D11, D3D12 and OpenGL applications render on the GPU, with no app-local graphics DLLs, and show in a console screenshot | For each API, x64 and x86: the probe exits 0 and its readback matches; the adapter is not a software renderer and is ours (`Yttrium VirtIO GPU 3D controller` for D3D, Zink on `Virtio-GPU Venus` for OpenGL; `Hardsoft VirtIO GPU 3D controller` before the November base); no module loads from the exe's directory; every graphics module comes from System32, SysWOW64, a subdirectory of either, or the driver store; the process loads our stack (`viogpu_d3d10.dll`; `d3d12.dll` with `vmlab-win3d\vkd3dcore.dll` and `libvulkan_virtio.dll`; `viogpu_wgl.dll` with `libvulkan_virtio.dll`); and at least one console capture shows the probe window's three marker squares |
| 2. `dxdiag` reports hardware acceleration | `Get-Dxdiag.ps1`'s `hardware_acceleration` is true, which requires D3D and DirectDraw `Enabled`, a full (not display-only) device, and not the Basic Render Driver. The card or driver must also be `viogpu3d` / Hardsoft |
| 3. `docs/` explains install, uninstall, the stack and pins, signing, known limits and the evidence | Not checked by the script; see "Done item 3" below |

Rows marked "extra" are reported but do not decide the result: the x86
vulkaninfo, exactly one Venus device in the x64 vulkaninfo (added for the
NVIDIA runs, so the AMD tables below predate it), the LunarG cross-check, the Vulkan probes, the device state after
install, and dxdiag's Direct3D status on its own.

## Host stack

Each run records the host in [docs/evidence/acceptance/<template>/host.txt](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/acceptance/).
For the 0.1.1-alpha-rc3 runs (2026-10-19), which made every current table
below except windows-10 on AMD1 (rc2, vmlab 0.28.0-alpha, Resizable BAR
off):

| Item | Value |
| --- | --- |
| Host GPUs used for Venus | `AMD1`: AMD Granite Ridge iGPU, PCI `0000:72:00.0`, RADV. `Nvidia1`: RTX 5090, `0000:01:00.0`. `Nvidia2`: RTX 5090, `0000:05:00.0`. vmlab resolves each GPU's render node from its address at run time (`/sys/class/drm/renderD*/device`; the numbers change between boots), and each run's "Host GPU:" line below records the node it had |
| QEMU | AMD1: 11.1.1 (`qemu-full 11.1.1-1`) from `PATH`. Nvidia1 and Nvidia2: the host config's `qemu_binary`, `~/.local/opt/qemu-vmlab-egl`, QEMU 11.1.50 (VMLabDev/qemu `d8a6048aae11`, the commit [patches/host/PINS](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/patches/host/PINS) pins) |
| virglrenderer | 1.3.0 (`1.3.0-2.1`) |
| Mesa / RADV | 26.2.2 (`mesa 3:26.2.2-2`, `vulkan-radeon 3:26.2.2-2`) |
| NVIDIA driver | 610.57.04 (`nvidia-utils 610.57.04-1`) |
| RTX 5090 BAR1 | 32 GiB on both cards: Resizable BAR on since 2026-10-19 ([docs/evidence/rc3/rebar/nvidia-smi-memory.txt](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/rc3/rebar/nvidia-smi-memory.txt)). Earlier NVIDIA runs had 256 MiB |
| NVIDIA render server | Nvidia1 and Nvidia2: the host config's `qemu_env` `RENDER_SERVER_EXEC_PATH`, `~/.local/opt/virglrenderer-vmlab` (virglrenderer 1.3.0 with [patches/host/virglrenderer/0001](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/patches/host/virglrenderer/0001-vkr-log-image-creation.patch) and `0002`) |
| vmlab | 0.30.0-alpha |
| Kernel | 7.2.3-1-cachyos |

`status-v.txt` records the GPU vmlab picked: `host_gpu=AMD1`, or
`host_gpu=Nvidia1` / `Nvidia2` with
`qemu=/home/wil/.local/opt/qemu-vmlab-egl/bin/qemu-system-x86_64`.
`qemu-argv-env.txt` has QEMU's command line, its environment (on NVIDIA
with `RENDER_SERVER_EXEC_PATH`) and the `vmlab-pci-filter:` line. Each NVIDIA run kept only its own card (`kept
NVIDIA GeForce RTX 5090, hid 1`), and every guest listed exactly one Venus
device.

## Results

### November base (2026-11-19)

The package rebased to Yttrium's November 2026 release (kernel driver
`e9917ae2`, Mesa `76c095bb`, driver version `100.6.101.58105`, bundle
`vmlab-win3d-0.1.3`), on `hypervisor = "vmlab"` with vmlab and the vmlab-hv
bundle built from the same commit, virglrenderer 1.3.0 with the
repository root's virglrenderer patches 0001 to 0003, NVIDIA driver
615.78.08, Mesa 26.2.6 on the host. Template `x86_64/windows-11@26100.1742.11`. `windows-10` was not
run.

| Run | Result | D3D9 x64, x86 | D3D11 x64, x86 | D3D12 x64, x86 | OpenGL x64, x86 | Vulkan x64, x86 |
| --- | --- | --- | --- | --- | --- | --- |
| `windows-11`, `AMD1` (Radeon 610M, RADV) | 12 of 12 | 59.58, 58.32 fps | 58.6, 58.85 fps | 55.39, 55.55 fps | 65.87, 64.34 fps | 58.87, 58.73 fps |
| `windows-11-nvidia2`, `Nvidia2` (RTX 5090) | 12 of 12 | 57.85, 56.72 fps | 57.34, 57.69 fps | 52.87, 52.99 fps | 64.14, 63.35 fps | 56.44, 56.31 fps |

The kernel log has no `NVRM: Xid` line before or after the runs. dxdiag
names the card "Yttrium VirtIO GPU 3D controller", with Direct3D and
DirectDraw enabled, WDDM 2.0. A first `AMD1` run passed every probe and
counted 11 of 12, because `tests/e2e/check.py` still looked for the old
adapter name in the dxdiag row. The table has the run after that fix.

After each run, in the same lab, the reduction page of GitHub #233 (a red
quad from a `STATIC_DRAW`, a `DYNAMIC_DRAW` and a `STREAM_DRAW` vertex
buffer) was opened in the guest's Edge 157.0.4341.60, whose WebGL renderer
was `ANGLE (VirtIO, Yttrium VirtIO GPU 3D controller (0x00001050) Direct3D11
vs_5_0 ps_5_0, D3D11)`. All three lines read `255,0,0,255` with `glError 0`
on both GPUs. The page is now `tests/e2e/adapter/pages/svb.html`, a check
of the adapter suite.

The runs of 2026-11-20 with `--apps` ([Adapter suite](#adapter-suite)) also
ran the six stretch applications on this base, on AMD1 and on Nvidia2. Each
ran until it was closed at its time limit. Their captures were not judged.
The compatibility sweep and the feature probes were not run on this base.

### Release candidate 0.1.1-alpha-rc3 (NVIDIA with Resizable BAR)

**Release candidate 0.1.1-alpha-rc3 passes every Done check in items 1 and
2 on both RTX 5090s, with helios's new Resizable BAR, and on AMD1: four
runs, 12 of 12 each.** BAR1 is now 32 GiB on both cards. NVIDIA's Vulkan
driver offers the device-local, host-visible type on the VRAM heap, so
`patches/mesa/venus-hide-host-access-to-small-bar-heap.patch` no longer hides it, and Venus guests on NVIDIA now use
VRAM-mapped memory. These are the first NVIDIA runs with that behaviour.
[docs/evidence/rc3/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/rc3/README.md) has the release record and
every result below in more detail.

- `scripts/release.sh 0.1.1-alpha-rc3` built the package from a fresh clone
  of `work/rc3` at [ee9ce49](https://github.com/VMLabDev/vmlab-win3d/commit/ee9ce49fefc7d6820653b6014d44d876fc12965d), with the shared test certificate
  (`WIN3D_SIGNING_DIR`), on the first attempt. The guest package has the
  same sources as rc2 ([0d3eb68](https://github.com/VMLabDev/vmlab-win3d/commit/0d3eb68b7780c21061b34c9768df08aa418497e4)); only [patches/host/build-virglrenderer.sh](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/patches/host/build-virglrenderer.sh)
  and documentation have changed since. QEMU is the same pinned commit,
  `d8a6048aae11`.
- Assets ([docs/evidence/rc3/release/SHA256SUMS](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/rc3/release/SHA256SUMS)):
  `vmlab-win3d-0.1.1-alpha-rc3-windows-x64.zip`
  `e39728c8d0ee24c81c91e86033b90349b3dc995d8a0ea55d806ba389df59917c`;
  `qemu-vmlab-egl-0.1.1-alpha-rc3-x86_64-linux.tar.zst`
  `b244b68a565e038f14ab81dd0a77e3899dd4ba6bf5240e81139e00edbb0484d2`;
  `virglrenderer-vmlab-0.1.1-alpha-rc3-x86_64-linux.tar.zst`
  `3bb8a796be1837ac2c0c3e247ccd1360be2a3f8bfd295a308e6462bec53d3fd9`;
  `RELEASE-NOTES.md`
  `885ac9357546494b9c4c25a7aa43f1306b98a59cb3f37f8fc872cad42172754d`;
  `SHA256SUMS`
  `98f1bb9e1ef4b3c4e9140d8dc5c780ca0c605d4d3b273a62f622b3143ce1475b`.
  The zip holds the same `manifest.json`
  (`b36cbda671e192ad27659157d6d19707407d43cf872cc2d3a6f79d61ac770ef0`,
  commit [ee9ce49](https://github.com/VMLabDev/vmlab-win3d/commit/ee9ce49fefc7d6820653b6014d44d876fc12965d)) as the bundle every run installed (`build.txt`, mode
  `reused`, built at [ee9ce49](https://github.com/VMLabDev/vmlab-win3d/commit/ee9ce49fefc7d6820653b6014d44d876fc12965d)).
- The NVIDIA runs used helios's host config: the patched QEMU
  (`qemu=/home/wil/.local/opt/qemu-vmlab-egl/bin/qemu-system-x86_64`, the
  pinned commit) and the vmlab render server
  (`RENDER_SERVER_EXEC_PATH=/home/wil/.local/opt/virglrenderer-vmlab/libexec/virgl_render_server`,
  byte-identical to the rc3 asset). Each run kept only its own card
  (`vmlab-pci-filter: … kept NVIDIA GeForce RTX 5090, hid 1`), and every
  guest listed exactly one Venus device.

| Template | Nvidia1 | Nvidia2 | AMD1 |
| --- | --- | --- | --- |
| windows-11 | **12 of 12**; every app renders | **12 of 12**; apps render, Blender's kept capture black | **12 of 12** (sanity); apps render, Blender's kept capture black |
| windows-10 | **12 of 12**; every app renders, Blender opaque (second attempt) | not run | not rerun; rc2's run stands |

- **Blender's kept capture** is black on windows-11 Nvidia2 and AMD1: the
  capture from two thirds of the way in shows its window before it has drawn.
  Rerun in the kept Nvidia2 lab, Blender showed the splash and then the full
  UI in every capture after the second, so it renders. The first launch in
  a fresh guest was most likely still starting
  ([rc3 README](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/rc3/README.md#blenders-kept-capture-is-black-on-windows-11-nvidia2-and-amd1)).
- **The first windows-10 Nvidia1 attempt hung in `vmlab cp`** during the
  install: 16 of 29 files copied, with no progress for 20 minutes. It was
  stopped and rerun in a fresh lab, which passed. That is a vmlab problem; the
  evidence for a vmlab report is in
  [docs/evidence/rc3/vmlab-cp-hang/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/rc3/vmlab-cp-hang/README.md).
- **Start-up was slow on windows-11**, on AMD1 as well as NVIDIA: the four
  windows-11 runs, the compatibility sweep and a load lab booted together,
  and some first probe launches waited 8 to 53 s for Defender's cloud check
  ([known limits](known-limits.md#the-first-d3d12-launch-in-a-new-guest-waits-on-defenders-cloud-check)).
  The probes count their 20 s from the first frame, so all passed, but
  their `avg_fps` in the tables below includes the wait. Measured from the
  first frame, on windows-11 D3D9, D3D11 and Vulkan run at 60 fps on both
  cards, OpenGL at 65 to 68 and D3D12 at 46 to 56. In rc1 the windows-11
  D3D9 probe ran at 43 to 52 fps on NVIDIA. On windows-10 Nvidia1, D3D9
  now runs at 51 to 52 fps, up from 27 and 46
  ([rc3 README](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/rc3/README.md#e2e-runs)).

The DirectDraw fullscreen and fixed-function suites ran on Nvidia1, with
44 to 57 flips per second at 32 bpp (26 to 44 in rc1) and the known
fixed-function results. The compatibility sweep passed **12 of 12 on
Nvidia2**, up from 5 of 12
([`compat.md`](compat.md#release-candidate-011-alpha-rc3-on-nvidia2)). The
**ReBAR load check** ran three Windows VMs on Nvidia2 at once: the e2e
run, the sweep and a probe loop, then a 15-minute stage with three probe
loops. The host logged **no `can't alloc VA space` line and no Xid**.
BAR1 peaked at 764 MiB of 32 GiB, and all 116 probe runs of the final
stage passed ([rc3 README](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/rc3/README.md#rebar-load-check)).

The rc3 runs replaced the evidence in `windows-11-nvidia1/`,
`windows-11-nvidia2/`, `windows-10-nvidia1/` and `windows-11/` (AMD1). The
earlier runs are in git history at [ee9ce49](https://github.com/VMLabDev/vmlab-win3d/commit/ee9ce49fefc7d6820653b6014d44d876fc12965d): rc1 on Nvidia1, the nvva
proof run on Nvidia2 and rc2 on AMD1.

### Release candidate 0.1.1-alpha-rc2 (AMD1)

**Release candidate 0.1.1-alpha-rc2 passes every Done check in items 1 and
2 on AMD1, on both templates: two runs, 12 of 12 each, and every stretch
application renders.** The NVIDIA part of rc2 ran after helios's
Resizable BAR change (`docs/plan.md`, D5), as release candidate rc3
(above). The rc3 sanity run on AMD1 replaced rc2's `windows-11/`
evidence; rc2's `windows-10/` evidence stands.

- `scripts/release.sh 0.1.1-alpha-rc2` built the package from a fresh clone
  of `main` at [0d3eb68](https://github.com/VMLabDev/vmlab-win3d/commit/0d3eb68b7780c21061b34c9768df08aa418497e4), with the shared test certificate
  (`WIN3D_SIGNING_DIR`). Since rc1 it adds Mesa patches 0031, 0032, 0043,
  0044, 0060, 0065 to 0067, 0070, 0075, 0080, 0085, 0086 and 0090 to 0092,
  vkd3d-proton 0006, and the render-server asset (virglrenderer 1.3.0 with
  [patches/host/virglrenderer/0001](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/patches/host/virglrenderer/0001-vkr-log-image-creation.patch) and `0002`). QEMU is the same pinned
  commit, `d8a6048aae11`. [docs/evidence/rc2/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/rc2/) has the release record.
- Assets ([docs/evidence/rc2/release/SHA256SUMS](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/rc2/release/SHA256SUMS)):
  `vmlab-win3d-0.1.1-alpha-rc2-windows-x64.zip`
  `16450f1959179df0957f088b4dabe845b4387e0cb17e837e2f70e80822a1bee0`;
  `qemu-vmlab-egl-0.1.1-alpha-rc2-x86_64-linux.tar.zst`
  `fc16d08cdd66fcfebd99e5c342f3a07b6fd0351c7b2e4012cb19c64460556678`;
  `virglrenderer-vmlab-0.1.1-alpha-rc2-x86_64-linux.tar.zst`
  `3355ffe48d72ad895b853fa60cd58c274d0a23fc6aefa597a16ae6e8a89145c7`;
  `RELEASE-NOTES.md`
  `0ea1aad50a2b3dc8c8d81d64e1f9a3bc3836f5ba8b753265ee5ab4182dbac6e0`.
  The zip holds the same `manifest.json`
  (`7de55b27748d335e0cd47d4b0d52c0a8faeb5a3295ac8de87495b5d0fac658f2`,
  commit [0d3eb68](https://github.com/VMLabDev/vmlab-win3d/commit/0d3eb68b7780c21061b34c9768df08aa418497e4)) as the bundle both runs installed.
- Both runs reused that bundle (`build.txt`, mode `reused`, built at
  [0d3eb68](https://github.com/VMLabDev/vmlab-win3d/commit/0d3eb68b7780c21061b34c9768df08aa418497e4)). They record commit [66809ce](https://github.com/VMLabDev/vmlab-win3d/commit/66809ce4ec3ff77158503fd64cd252c2c5d80e19), which adds only documentation.
- The two runs shared AMD1: they started 20 s apart, so their probes ran
  with two VMs on the iGPU. The windows-11 D3D12 and x86 D3D9 rates are
  lower than rc1's for that reason (table below); the probes still pass.

| Template | AMD1 (rc2) |
| --- | --- |
| windows-11 | **12 of 12**; every app renders |
| windows-10 | **12 of 12**; every app renders, Blender opaque |

Probe frame rates (x64, x86) and `startup_seconds`, the time from the
probe's start to its first frame (`probes/<arch>-<api>_probe.json`):

| Probe | w11 fps | w11 `startup_seconds` | w10 fps | w10 `startup_seconds` | rc1 w11 / w10 fps |
| --- | --- | --- | --- | --- | --- |
| D3D9 | 59.7, 52.2 | 0.13, 3.01 | 59.7, 59.5 | 0.12, 0.17 | 59.6, 58.4 / 59.2, 59.3 |
| D3D11 | 59.9, 59.8 | 0.10, 0.12 | 59.9, 59.8 | 0.09, 0.14 | 59.9, 59.9 / 59.8, 59.8 |
| D3D12 | 51.5, 43.3 | 3.24, 5.96 | 58.6, 59.3 | 0.45, 0.24 | 58.1, 58.2 / 57.1, 57.7 |
| OpenGL | 57.5, 63.8 | 3.67, 1.46 | 67.6, 68.0 | 0.33, 0.20 | 65.7, 65.1 / 67.0, 67.4 |
| Vulkan | 59.8, 59.7 | 0.15, 0.15 | 59.8, 59.9 | 0.13, 0.11 | 59.8, 59.7 / 59.7, 59.8 |

On windows-11 four probes took 1.5 to 6 s to their first frame, the
longest x86 D3D12. That is the first load of new unsigned images, which
Defender holds until its cloud check answers ([known
limits](known-limits.md#the-first-d3d12-launch-in-a-new-guest-waits-on-defenders-cloud-check));
[`x86d3d12.md`](x86d3d12.md) measured 2.2 to 7.2 s for x86 D3D12 in the
same step. On windows-10 every probe started in under 0.5 s.

The DirectDraw fullscreen and both D3D9 fixed-function suites ran in the
kept windows-11 lab with the results of rc1 or better, and the
compatibility sweep, in its own lab, passed 12 of 12 on AMD1, up from 10
of 12 ([docs/evidence/rc2/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/rc2/README.md),
[`compat.md`](compat.md#release-candidate-011-alpha-rc2-on-amd1)).

### Release candidate 0.1.1-alpha-rc1

**Release candidate 0.1.1-alpha-rc1 passes every Done check in items 1 and
2, on both templates and all three host GPUs: five runs, 12 of 12 each.**
All five ran one package:

- `scripts/release.sh 0.1.1-alpha-rc1` built it from a fresh clone of commit
  [0ee5ba1](https://github.com/VMLabDev/vmlab-win3d/commit/0ee5ba149e10b7671aef8ab3db71f187c53d895c) (main at [82d8e51](https://github.com/VMLabDev/vmlab-win3d/commit/82d8e510c18cf4eaad92bef85ec1ccf14fdd0e75), plus `installer/VERSION` 0.1.1 and a
  `release.sh` version pattern that accepts `-rc1`), with the shared test
  certificate (`WIN3D_SIGNING_DIR`). Mesa patches 0001 to 0017, 0020 to
  0027, 0030, 0035 and 0040 to 0042; KMD patches 0001 and 0002; the
  installer's FeatureScore and device pin; QEMU at the pinned master-based
  commit. [docs/evidence/rc/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/rc/) has the release record.
- Assets ([docs/evidence/rc/release/SHA256SUMS](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/rc/release/SHA256SUMS)):
  `vmlab-win3d-0.1.1-alpha-rc1-windows-x64.zip`
  `150406672d6e43373c643e940acf97337e058e094f45ea2c4eed2660e85205f0`;
  `qemu-vmlab-egl-0.1.1-alpha-rc1-x86_64-linux.tar.zst`
  `940ceefdd96966952526bc5c793828e4b970eb761b110d69edc984690c601be5`;
  `RELEASE-NOTES.md`
  `b27109e8098fb352bd453b2da80e832d3e4c25a21fe844960b3278c29faab67e`.
  The zip holds the same `manifest.json`
  (`b9523c1b4c1127300288530ef684434fa56d947057b780b01ac2ce2d6c097339`) as
  the bundle the runs installed.
- Every run reused that bundle (`build.txt`, mode `reused`, built at
  [0ee5ba1](https://github.com/VMLabDev/vmlab-win3d/commit/0ee5ba149e10b7671aef8ab3db71f187c53d895c)). Runs that started after [0c0788c](https://github.com/VMLabDev/vmlab-win3d/commit/0c0788ccbae9964f95a13702af90aacebdc8f52a) record that commit; it
  changes only `tests/e2e/` (below).

| Template | AMD1 | Nvidia1 | Nvidia2 |
| --- | --- | --- | --- |
| windows-11 | **12 of 12**; apps render | **12 of 12**; apps render | **12 of 12**; apps render |
| windows-10 | **12 of 12**; apps render | **12 of 12**; apps render | not run |

The rc2 and rc3 runs replaced rc1's evidence. rc1's AMD1 evidence is in
git history at [0d3eb68](https://github.com/VMLabDev/vmlab-win3d/commit/0d3eb68b7780c21061b34c9768df08aa418497e4), and its Nvidia1 evidence at [ee9ce49](https://github.com/VMLabDev/vmlab-win3d/commit/ee9ce49fefc7d6820653b6014d44d876fc12965d). The rc1
Nvidia2 run, with its apps, is in git history at [f1c5219](https://github.com/VMLabDev/vmlab-win3d/commit/f1c5219217a4d4c9317fb2d48a423dce0b6a9fe0). The nvva
worker's proof run on Nvidia2 (package built at [a9579f1](https://github.com/VMLabDev/vmlab-win3d/commit/a9579f13c64cb2fea28114541480fc6e64fae7df), Mesa 0070 and
0086, no apps step; [`nvva.md`](nvva.md)) replaced it, and rc3 then
replaced that run.

The windows-11 runs lost their first `--apps` pass to slow downloads, and
the AMD1 and Nvidia2 runs were restarted once:

- Run in parallel, the first windows-11 runs on AMD1 and Nvidia2 wrote the
  same download file, and the LunarG runtime failed its sha256 check. They
  were stopped before the install, their labs destroyed, and both rerun
  from a fresh lab. [0c0788c](https://github.com/VMLabDev/vmlab-win3d/commit/0c0788ccbae9964f95a13702af90aacebdc8f52a) gives each run its own download file and its
  own apps staging directory.
- Helios's direct downloads from Microsoft and blender.org ran at about
  1 KB/s that night. The three windows-11 runs reached `--apps` before the
  DirectX SDK and Blender were in the cache, so their apps step was stopped
  after the Done evidence was collected, with the labs kept (`--keep`). The
  pinned files were then fetched through a proxy, from Blender mirrors for
  Blender, and checked against `tests/e2e/PINS`. After a reboot of each lab,
  `tests/e2e/apps.sh` ran against it (`apps/apps-rerun.log`). The
  windows-10 runs had the cache and ran `--apps` straight through.

<!-- e2e:results -->
### windows-11

Host GPU: AMD1, /dev/dri/renderD129 0000:72:00.0 VGA compatible controller: Advanced Micro Devices, Inc. [AMD/ATI] Granite Ridge [Radeon Graphics] (rev c1). QEMU: QEMU emulator version 11.1.1 (from PATH).

Run 2026-10-19T17:54:59+10:00 at commit [ee9ce49fefc7](https://github.com/VMLabDev/vmlab-win3d/commit/ee9ce49fefc7d6820653b6014d44d876fc12965d), template `x86_64/windows-11@26100.1742.5`. **PASS: 12 of 12 Done checks pass** (run stage: ok). Evidence: [docs/evidence/acceptance/windows-11/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/acceptance/windows-11/); `results.txt` is the script's own table.

| Done item | Check | Result | Evidence | Note |
| --- | --- | --- | --- | --- |
| 1 | scripts/build.sh builds the package | **pass** | `build.txt` | reused; exit 0 |
| 2 | documented install command (scripts/install-into-vm.sh) | **pass** | `install.log` | exit 0; Installed |
| 2 (extra) | display device on our driver, problem code 0 | **pass** | `installed-gpu-state.json` | Hardsoft VirtIO GPU 3D controller |
| 2 | vulkaninfo --summary (x64) lists the Venus device | **pass** | `vulkaninfo/vulkaninfo-x64.txt` | Virtio-GPU Venus (AMD Ryzen 9 9950X 16-Core Processor (RADV RAPHAEL_MENDOCINO)) |
| 2 (extra) | vulkaninfo --summary (x86) lists the Venus device | **pass** | `vulkaninfo/vulkaninfo-x86.txt` | Virtio-GPU Venus (AMD Ryzen 9 9950X 16-Core Processor (RADV RAPHAEL_MENDOCINO)) |
| 2 (extra) | vulkaninfo --summary (x64) lists exactly one Venus device | **pass** | `vulkaninfo/vulkaninfo-x64.txt` | 1 Venus: Virtio-GPU Venus (AMD Ryzen 9 9950X 16-Core Processor (RADV RAPHAEL_MENDOCINO)) |
| 2 (extra) | LunarG vulkaninfo 1.4.363 (x64, pinned) lists Venus through the installed loader | **pass** | `vulkaninfo/lunarg-vulkaninfo-x64.txt` | Virtio-GPU Venus (AMD Ryzen 9 9950X 16-Core Processor (RADV RAPHAEL_MENDOCINO)) |
| 2 (extra) | LunarG vulkaninfo 1.4.363 (x86, pinned) lists Venus through the installed loader | **pass** | `vulkaninfo/lunarg-vulkaninfo-x86.txt` | Virtio-GPU Venus (AMD Ryzen 9 9950X 16-Core Processor (RADV RAPHAEL_MENDOCINO)) |
| 2 | Direct3D 9 (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-d3d9_probe.run.json`, `probes/x64-d3d9_probe.modules.txt`, `console-x64-d3d9_probe.png` | Hardsoft VirtIO GPU 3D controller, 59.44 fps, readback ok, 5 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 9 (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-d3d9_probe.run.json`, `probes/x86-d3d9_probe.modules.txt`, `console-x86-d3d9_probe.png` | Hardsoft VirtIO GPU 3D controller, 33.06 fps, readback ok, 5 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 11 (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-d3d11_probe.run.json`, `probes/x64-d3d11_probe.modules.txt`, `console-x64-d3d11_probe.png` | Hardsoft VirtIO GPU 3D controller, 59.53 fps, readback ok, 6 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 11 (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-d3d11_probe.run.json`, `probes/x86-d3d11_probe.modules.txt`, `console-x86-d3d11_probe.png` | Hardsoft VirtIO GPU 3D controller, 59.24 fps, readback ok, 6 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 12 (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-d3d12_probe.run.json`, `probes/x64-d3d12_probe.modules.txt`, `console-x64-d3d12_probe.png` | Hardsoft VirtIO GPU 3D controller, 36.66 fps, readback ok, 11 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 12 (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-d3d12_probe.run.json`, `probes/x86-d3d12_probe.modules.txt`, `console-x86-d3d12_probe.png` | Hardsoft VirtIO GPU 3D controller, 17.28 fps, readback ok, 11 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | OpenGL (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-opengl_probe.run.json`, `probes/x64-opengl_probe.modules.txt`, `console-x64-opengl_probe.png` | zink Vulkan 1.4(Virtio-GPU Venus (AMD Ryzen 9 9950X 16-Core Processor (RADV RAPHAEL_MENDOCINO)) (MESA_RADV)), 36.89 fps, readback ok, 10 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | OpenGL (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-opengl_probe.run.json`, `probes/x86-opengl_probe.modules.txt`, `console-x86-opengl_probe.png` | zink Vulkan 1.4(Virtio-GPU Venus (AMD Ryzen 9 9950X 16-Core Processor (RADV RAPHAEL_MENDOCINO)) (MESA_RADV)), 37.04 fps, readback ok, 10 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 (extra) | Vulkan (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-vulkan_probe.run.json`, `probes/x64-vulkan_probe.modules.txt`, `console-x64-vulkan_probe.png` | Virtio-GPU Venus (AMD Ryzen 9 9950X 16-Core Processor (RADV RAPHAEL_MENDOCINO)), 59.48 fps, readback ok, 7 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 (extra) | Vulkan (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-vulkan_probe.run.json`, `probes/x86-vulkan_probe.modules.txt`, `console-x86-vulkan_probe.png` | Virtio-GPU Venus (AMD Ryzen 9 9950X 16-Core Processor (RADV RAPHAEL_MENDOCINO)), 59.49 fps, readback ok, 7 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | dxdiag reports hardware acceleration on our driver | **pass** | `dxdiag/dxdiag.json` | hardware_acceleration=True, card 'Hardsoft VirtIO GPU 3D controller', D3D Enabled, DDraw Enabled, WDDM 2.0 |
| 2 (extra) | dxdiag: Direct3D acceleration on our full WDDM device | **pass** | `dxdiag/dxdiag.json` | D3D Status Enabled, Full Device (POST), WDDM 2.0 |

### windows-10

Host GPU: AMD1, /dev/dri/renderD129 0000:72:00.0 VGA compatible controller: Advanced Micro Devices, Inc. [AMD/ATI] Granite Ridge [Radeon Graphics] (rev c1). QEMU: QEMU emulator version 11.1.1 (from PATH).

Run 2026-10-19T13:06:39+10:00 at commit [66809ce4ec3f](https://github.com/VMLabDev/vmlab-win3d/commit/66809ce4ec3ff77158503fd64cd252c2c5d80e19), template `x86_64/windows-10@19045.2006.8`. **PASS: 12 of 12 Done checks pass** (run stage: ok). Evidence: [docs/evidence/acceptance/windows-10/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/acceptance/windows-10/); `results.txt` is the script's own table.

| Done item | Check | Result | Evidence | Note |
| --- | --- | --- | --- | --- |
| 1 | scripts/build.sh builds the package | **pass** | `build.txt` | reused; exit 0 |
| 2 | documented install command (scripts/install-into-vm.sh) | **pass** | `install.log` | exit 0; Installed |
| 2 (extra) | display device on our driver, problem code 0 | **pass** | `installed-gpu-state.json` | Hardsoft VirtIO GPU 3D controller |
| 2 | vulkaninfo --summary (x64) lists the Venus device | **pass** | `vulkaninfo/vulkaninfo-x64.txt` | Virtio-GPU Venus (AMD Ryzen 9 9950X 16-Core Processor (RADV RAPHAEL_MENDOCINO)) |
| 2 (extra) | vulkaninfo --summary (x86) lists the Venus device | **pass** | `vulkaninfo/vulkaninfo-x86.txt` | Virtio-GPU Venus (AMD Ryzen 9 9950X 16-Core Processor (RADV RAPHAEL_MENDOCINO)) |
| 2 (extra) | vulkaninfo --summary (x64) lists exactly one Venus device | **pass** | `vulkaninfo/vulkaninfo-x64.txt` | 1 Venus: Virtio-GPU Venus (AMD Ryzen 9 9950X 16-Core Processor (RADV RAPHAEL_MENDOCINO)) |
| 2 (extra) | LunarG vulkaninfo 1.4.363 (x64, pinned) lists Venus through the installed loader | **pass** | `vulkaninfo/lunarg-vulkaninfo-x64.txt` | Virtio-GPU Venus (AMD Ryzen 9 9950X 16-Core Processor (RADV RAPHAEL_MENDOCINO)) |
| 2 (extra) | LunarG vulkaninfo 1.4.363 (x86, pinned) lists Venus through the installed loader | **pass** | `vulkaninfo/lunarg-vulkaninfo-x86.txt` | Virtio-GPU Venus (AMD Ryzen 9 9950X 16-Core Processor (RADV RAPHAEL_MENDOCINO)) |
| 2 | Direct3D 9 (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-d3d9_probe.run.json`, `probes/x64-d3d9_probe.modules.txt`, `console-x64-d3d9_probe.png` | Hardsoft VirtIO GPU 3D controller, 59.71 fps, readback ok, 4 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 9 (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-d3d9_probe.run.json`, `probes/x86-d3d9_probe.modules.txt`, `console-x86-d3d9_probe.png` | Hardsoft VirtIO GPU 3D controller, 59.52 fps, readback ok, 4 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 11 (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-d3d11_probe.run.json`, `probes/x64-d3d11_probe.modules.txt`, `console-x64-d3d11_probe.png` | Hardsoft VirtIO GPU 3D controller, 59.88 fps, readback ok, 6 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 11 (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-d3d11_probe.run.json`, `probes/x86-d3d11_probe.modules.txt`, `console-x86-d3d11_probe.png` | Hardsoft VirtIO GPU 3D controller, 59.75 fps, readback ok, 6 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 12 (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-d3d12_probe.run.json`, `probes/x64-d3d12_probe.modules.txt`, `console-x64-d3d12_probe.png` | Hardsoft VirtIO GPU 3D controller, 58.62 fps, readback ok, 11 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 12 (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-d3d12_probe.run.json`, `probes/x86-d3d12_probe.modules.txt`, `console-x86-d3d12_probe.png` | Hardsoft VirtIO GPU 3D controller, 59.26 fps, readback ok, 11 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | OpenGL (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-opengl_probe.run.json`, `probes/x64-opengl_probe.modules.txt`, `console-x64-opengl_probe.png` | zink Vulkan 1.4(Virtio-GPU Venus (AMD Ryzen 9 9950X 16-Core Processor (RADV RAPHAEL_MENDOCINO)) (MESA_RADV)), 67.57 fps, readback ok, 10 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | OpenGL (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-opengl_probe.run.json`, `probes/x86-opengl_probe.modules.txt`, `console-x86-opengl_probe.png` | zink Vulkan 1.4(Virtio-GPU Venus (AMD Ryzen 9 9950X 16-Core Processor (RADV RAPHAEL_MENDOCINO)) (MESA_RADV)), 67.97 fps, readback ok, 10 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 (extra) | Vulkan (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-vulkan_probe.run.json`, `probes/x64-vulkan_probe.modules.txt`, `console-x64-vulkan_probe.png` | Virtio-GPU Venus (AMD Ryzen 9 9950X 16-Core Processor (RADV RAPHAEL_MENDOCINO)), 59.82 fps, readback ok, 7 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 (extra) | Vulkan (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-vulkan_probe.run.json`, `probes/x86-vulkan_probe.modules.txt`, `console-x86-vulkan_probe.png` | Virtio-GPU Venus (AMD Ryzen 9 9950X 16-Core Processor (RADV RAPHAEL_MENDOCINO)), 59.89 fps, readback ok, 7 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | dxdiag reports hardware acceleration on our driver | **pass** | `dxdiag/dxdiag.json` | hardware_acceleration=True, card 'Hardsoft VirtIO GPU 3D controller', D3D Enabled, DDraw Enabled, WDDM 2.0 |
| 2 (extra) | dxdiag: Direct3D acceleration on our full WDDM device | **pass** | `dxdiag/dxdiag.json` | D3D Status Enabled, Full Device (POST), WDDM 2.0 |

### windows-10-nvidia1

Host GPU: Nvidia1, /dev/dri/renderD128 0000:01:00.0 VGA compatible controller: NVIDIA Corporation GB202 [GeForce RTX 5090] (rev a1). QEMU: /home/wil/.local/opt/qemu-vmlab-egl/bin/qemu-system-x86_64, QEMU emulator version 11.1.50 (vmlab d8a6048aae11) (the Nvidia1 qemu_binary).

Run 2026-10-19T18:34:28+10:00 at commit [ee9ce49fefc7](https://github.com/VMLabDev/vmlab-win3d/commit/ee9ce49fefc7d6820653b6014d44d876fc12965d), template `x86_64/windows-10@19045.2006.8`. **PASS: 12 of 12 Done checks pass** (run stage: ok). Evidence: [docs/evidence/acceptance/windows-10-nvidia1/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/acceptance/windows-10-nvidia1/); `results.txt` is the script's own table.

| Done item | Check | Result | Evidence | Note |
| --- | --- | --- | --- | --- |
| 1 | scripts/build.sh builds the package | **pass** | `build.txt` | reused; exit 0 |
| 2 | documented install command (scripts/install-into-vm.sh) | **pass** | `install.log` | exit 0; Installed |
| 2 (extra) | display device on our driver, problem code 0 | **pass** | `installed-gpu-state.json` | Hardsoft VirtIO GPU 3D controller |
| 2 | vulkaninfo --summary (x64) lists the Venus device | **pass** | `vulkaninfo/vulkaninfo-x64.txt` | Virtio-GPU Venus (NVIDIA GeForce RTX 5090) |
| 2 (extra) | vulkaninfo --summary (x86) lists the Venus device | **pass** | `vulkaninfo/vulkaninfo-x86.txt` | Virtio-GPU Venus (NVIDIA GeForce RTX 5090) |
| 2 (extra) | vulkaninfo --summary (x64) lists exactly one Venus device | **pass** | `vulkaninfo/vulkaninfo-x64.txt` | 1 Venus: Virtio-GPU Venus (NVIDIA GeForce RTX 5090) |
| 2 (extra) | LunarG vulkaninfo 1.4.363 (x64, pinned) lists Venus through the installed loader | **pass** | `vulkaninfo/lunarg-vulkaninfo-x64.txt` | Virtio-GPU Venus (NVIDIA GeForce RTX 5090) |
| 2 (extra) | LunarG vulkaninfo 1.4.363 (x86, pinned) lists Venus through the installed loader | **pass** | `vulkaninfo/lunarg-vulkaninfo-x86.txt` | Virtio-GPU Venus (NVIDIA GeForce RTX 5090) |
| 2 | Direct3D 9 (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-d3d9_probe.run.json`, `probes/x64-d3d9_probe.modules.txt`, `console-x64-d3d9_probe.png` | Hardsoft VirtIO GPU 3D controller, 48.05 fps, readback ok, 4 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 9 (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-d3d9_probe.run.json`, `probes/x86-d3d9_probe.modules.txt`, `console-x86-d3d9_probe.png` | Hardsoft VirtIO GPU 3D controller, 48.17 fps, readback ok, 4 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 11 (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-d3d11_probe.run.json`, `probes/x64-d3d11_probe.modules.txt`, `console-x64-d3d11_probe.png` | Hardsoft VirtIO GPU 3D controller, 54.36 fps, readback ok, 6 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 11 (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-d3d11_probe.run.json`, `probes/x86-d3d11_probe.modules.txt`, `console-x86-d3d11_probe.png` | Hardsoft VirtIO GPU 3D controller, 57.69 fps, readback ok, 6 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 12 (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-d3d12_probe.run.json`, `probes/x64-d3d12_probe.modules.txt`, `console-x64-d3d12_probe.png` | Hardsoft VirtIO GPU 3D controller, 48.09 fps, readback ok, 11 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 12 (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-d3d12_probe.run.json`, `probes/x86-d3d12_probe.modules.txt`, `console-x86-d3d12_probe.png` | Hardsoft VirtIO GPU 3D controller, 50.28 fps, readback ok, 11 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | OpenGL (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-opengl_probe.run.json`, `probes/x64-opengl_probe.modules.txt`, `console-x64-opengl_probe.png` | zink Vulkan 1.4(Virtio-GPU Venus (NVIDIA GeForce RTX 5090) (NVIDIA_PROPRIETARY)), 64.01 fps, readback ok, 10 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | OpenGL (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-opengl_probe.run.json`, `probes/x86-opengl_probe.modules.txt`, `console-x86-opengl_probe.png` | zink Vulkan 1.4(Virtio-GPU Venus (NVIDIA GeForce RTX 5090) (NVIDIA_PROPRIETARY)), 61.23 fps, readback ok, 10 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 (extra) | Vulkan (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-vulkan_probe.run.json`, `probes/x64-vulkan_probe.modules.txt`, `console-x64-vulkan_probe.png` | Virtio-GPU Venus (NVIDIA GeForce RTX 5090), 55.55 fps, readback ok, 7 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 (extra) | Vulkan (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-vulkan_probe.run.json`, `probes/x86-vulkan_probe.modules.txt`, `console-x86-vulkan_probe.png` | Virtio-GPU Venus (NVIDIA GeForce RTX 5090), 54.23 fps, readback ok, 7 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | dxdiag reports hardware acceleration on our driver | **pass** | `dxdiag/dxdiag.json` | hardware_acceleration=True, card 'Hardsoft VirtIO GPU 3D controller', D3D Enabled, DDraw Enabled, WDDM 2.0 |
| 2 (extra) | dxdiag: Direct3D acceleration on our full WDDM device | **pass** | `dxdiag/dxdiag.json` | D3D Status Enabled, Full Device (POST), WDDM 2.0 |

### windows-11-nvidia1

Host GPU: Nvidia1, /dev/dri/renderD128 0000:01:00.0 VGA compatible controller: NVIDIA Corporation GB202 [GeForce RTX 5090] (rev a1). QEMU: /home/wil/.local/opt/qemu-vmlab-egl/bin/qemu-system-x86_64, QEMU emulator version 11.1.50 (vmlab d8a6048aae11) (the Nvidia1 qemu_binary).

Run 2026-10-19T17:54:47+10:00 at commit [ee9ce49fefc7](https://github.com/VMLabDev/vmlab-win3d/commit/ee9ce49fefc7d6820653b6014d44d876fc12965d), template `x86_64/windows-11@26100.1742.5`. **PASS: 12 of 12 Done checks pass** (run stage: ok). Evidence: [docs/evidence/acceptance/windows-11-nvidia1/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/acceptance/windows-11-nvidia1/); `results.txt` is the script's own table.

| Done item | Check | Result | Evidence | Note |
| --- | --- | --- | --- | --- |
| 1 | scripts/build.sh builds the package | **pass** | `build.txt` | reused; exit 0 |
| 2 | documented install command (scripts/install-into-vm.sh) | **pass** | `install.log` | exit 0; Installed |
| 2 (extra) | display device on our driver, problem code 0 | **pass** | `installed-gpu-state.json` | Hardsoft VirtIO GPU 3D controller |
| 2 | vulkaninfo --summary (x64) lists the Venus device | **pass** | `vulkaninfo/vulkaninfo-x64.txt` | Virtio-GPU Venus (NVIDIA GeForce RTX 5090) |
| 2 (extra) | vulkaninfo --summary (x86) lists the Venus device | **pass** | `vulkaninfo/vulkaninfo-x86.txt` | Virtio-GPU Venus (NVIDIA GeForce RTX 5090) |
| 2 (extra) | vulkaninfo --summary (x64) lists exactly one Venus device | **pass** | `vulkaninfo/vulkaninfo-x64.txt` | 1 Venus: Virtio-GPU Venus (NVIDIA GeForce RTX 5090) |
| 2 (extra) | LunarG vulkaninfo 1.4.363 (x64, pinned) lists Venus through the installed loader | **pass** | `vulkaninfo/lunarg-vulkaninfo-x64.txt` | Virtio-GPU Venus (NVIDIA GeForce RTX 5090) |
| 2 (extra) | LunarG vulkaninfo 1.4.363 (x86, pinned) lists Venus through the installed loader | **pass** | `vulkaninfo/lunarg-vulkaninfo-x86.txt` | Virtio-GPU Venus (NVIDIA GeForce RTX 5090) |
| 2 | Direct3D 9 (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-d3d9_probe.run.json`, `probes/x64-d3d9_probe.modules.txt`, `console-x64-d3d9_probe.png` | Hardsoft VirtIO GPU 3D controller, 56.34 fps, readback ok, 5 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 9 (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-d3d9_probe.run.json`, `probes/x86-d3d9_probe.modules.txt`, `console-x86-d3d9_probe.png` | Hardsoft VirtIO GPU 3D controller, 32.32 fps, readback ok, 5 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 11 (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-d3d11_probe.run.json`, `probes/x64-d3d11_probe.modules.txt`, `console-x64-d3d11_probe.png` | Hardsoft VirtIO GPU 3D controller, 57.8 fps, readback ok, 6 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 11 (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-d3d11_probe.run.json`, `probes/x86-d3d11_probe.modules.txt`, `console-x86-d3d11_probe.png` | Hardsoft VirtIO GPU 3D controller, 57.83 fps, readback ok, 6 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 12 (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-d3d12_probe.run.json`, `probes/x64-d3d12_probe.modules.txt`, `console-x64-d3d12_probe.png` | Hardsoft VirtIO GPU 3D controller, 32.66 fps, readback ok, 11 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 12 (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-d3d12_probe.run.json`, `probes/x86-d3d12_probe.modules.txt`, `console-x86-d3d12_probe.png` | Hardsoft VirtIO GPU 3D controller, 15.37 fps, readback ok, 11 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | OpenGL (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-opengl_probe.run.json`, `probes/x64-opengl_probe.modules.txt`, `console-x64-opengl_probe.png` | zink Vulkan 1.4(Virtio-GPU Venus (NVIDIA GeForce RTX 5090) (NVIDIA_PROPRIETARY)), 36.51 fps, readback ok, 10 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | OpenGL (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-opengl_probe.run.json`, `probes/x86-opengl_probe.modules.txt`, `console-x86-opengl_probe.png` | zink Vulkan 1.4(Virtio-GPU Venus (NVIDIA GeForce RTX 5090) (NVIDIA_PROPRIETARY)), 36.43 fps, readback ok, 10 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 (extra) | Vulkan (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-vulkan_probe.run.json`, `probes/x64-vulkan_probe.modules.txt`, `console-x64-vulkan_probe.png` | Virtio-GPU Venus (NVIDIA GeForce RTX 5090), 57.86 fps, readback ok, 7 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 (extra) | Vulkan (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-vulkan_probe.run.json`, `probes/x86-vulkan_probe.modules.txt`, `console-x86-vulkan_probe.png` | Virtio-GPU Venus (NVIDIA GeForce RTX 5090), 56.76 fps, readback ok, 7 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | dxdiag reports hardware acceleration on our driver | **pass** | `dxdiag/dxdiag.json` | hardware_acceleration=True, card 'Hardsoft VirtIO GPU 3D controller', D3D Enabled, DDraw Enabled, WDDM 2.0 |
| 2 (extra) | dxdiag: Direct3D acceleration on our full WDDM device | **pass** | `dxdiag/dxdiag.json` | D3D Status Enabled, Full Device (POST), WDDM 2.0 |

### windows-11-nvidia2

Host GPU: Nvidia2, /dev/dri/renderD130 0000:05:00.0 VGA compatible controller: NVIDIA Corporation GB202 [GeForce RTX 5090] (rev a1). QEMU: /home/wil/.local/opt/qemu-vmlab-egl/bin/qemu-system-x86_64, QEMU emulator version 11.1.50 (vmlab d8a6048aae11) (the Nvidia2 qemu_binary).

Run 2026-10-19T17:54:47+10:00 at commit [ee9ce49fefc7](https://github.com/VMLabDev/vmlab-win3d/commit/ee9ce49fefc7d6820653b6014d44d876fc12965d), template `x86_64/windows-11@26100.1742.5`. **PASS: 12 of 12 Done checks pass** (run stage: ok). Evidence: [docs/evidence/acceptance/windows-11-nvidia2/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/acceptance/windows-11-nvidia2/); `results.txt` is the script's own table.

| Done item | Check | Result | Evidence | Note |
| --- | --- | --- | --- | --- |
| 1 | scripts/build.sh builds the package | **pass** | `build.txt` | reused; exit 0 |
| 2 | documented install command (scripts/install-into-vm.sh) | **pass** | `install.log` | exit 0; Installed |
| 2 (extra) | display device on our driver, problem code 0 | **pass** | `installed-gpu-state.json` | Hardsoft VirtIO GPU 3D controller |
| 2 | vulkaninfo --summary (x64) lists the Venus device | **pass** | `vulkaninfo/vulkaninfo-x64.txt` | Virtio-GPU Venus (NVIDIA GeForce RTX 5090) |
| 2 (extra) | vulkaninfo --summary (x86) lists the Venus device | **pass** | `vulkaninfo/vulkaninfo-x86.txt` | Virtio-GPU Venus (NVIDIA GeForce RTX 5090) |
| 2 (extra) | vulkaninfo --summary (x64) lists exactly one Venus device | **pass** | `vulkaninfo/vulkaninfo-x64.txt` | 1 Venus: Virtio-GPU Venus (NVIDIA GeForce RTX 5090) |
| 2 (extra) | LunarG vulkaninfo 1.4.363 (x64, pinned) lists Venus through the installed loader | **pass** | `vulkaninfo/lunarg-vulkaninfo-x64.txt` | Virtio-GPU Venus (NVIDIA GeForce RTX 5090) |
| 2 (extra) | LunarG vulkaninfo 1.4.363 (x86, pinned) lists Venus through the installed loader | **pass** | `vulkaninfo/lunarg-vulkaninfo-x86.txt` | Virtio-GPU Venus (NVIDIA GeForce RTX 5090) |
| 2 | Direct3D 9 (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-d3d9_probe.run.json`, `probes/x64-d3d9_probe.modules.txt`, `console-x64-d3d9_probe.png` | Hardsoft VirtIO GPU 3D controller, 56.59 fps, readback ok, 5 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 9 (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-d3d9_probe.run.json`, `probes/x86-d3d9_probe.modules.txt`, `console-x86-d3d9_probe.png` | Hardsoft VirtIO GPU 3D controller, 32.1 fps, readback ok, 5 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 11 (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-d3d11_probe.run.json`, `probes/x64-d3d11_probe.modules.txt`, `console-x64-d3d11_probe.png` | Hardsoft VirtIO GPU 3D controller, 57.81 fps, readback ok, 6 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 11 (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-d3d11_probe.run.json`, `probes/x86-d3d11_probe.modules.txt`, `console-x86-d3d11_probe.png` | Hardsoft VirtIO GPU 3D controller, 57.78 fps, readback ok, 6 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 12 (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-d3d12_probe.run.json`, `probes/x64-d3d12_probe.modules.txt`, `console-x64-d3d12_probe.png` | Hardsoft VirtIO GPU 3D controller, 31.88 fps, readback ok, 11 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | Direct3D 12 (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-d3d12_probe.run.json`, `probes/x86-d3d12_probe.modules.txt`, `console-x86-d3d12_probe.png` | Hardsoft VirtIO GPU 3D controller, 14.57 fps, readback ok, 11 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | OpenGL (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-opengl_probe.run.json`, `probes/x64-opengl_probe.modules.txt`, `console-x64-opengl_probe.png` | zink Vulkan 1.4(Virtio-GPU Venus (NVIDIA GeForce RTX 5090) (NVIDIA_PROPRIETARY)), 45.16 fps, readback ok, 10 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | OpenGL (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-opengl_probe.run.json`, `probes/x86-opengl_probe.modules.txt`, `console-x86-opengl_probe.png` | zink Vulkan 1.4(Virtio-GPU Venus (NVIDIA GeForce RTX 5090) (NVIDIA_PROPRIETARY)), 44.6 fps, readback ok, 10 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 (extra) | Vulkan (x64) renders on the GPU and shows on the console | **pass** | `probes/x64-vulkan_probe.run.json`, `probes/x64-vulkan_probe.modules.txt`, `console-x64-vulkan_probe.png` | Virtio-GPU Venus (NVIDIA GeForce RTX 5090), 57.58 fps, readback ok, 7 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 (extra) | Vulkan (x86) renders on the GPU and shows on the console | **pass** | `probes/x86-vulkan_probe.run.json`, `probes/x86-vulkan_probe.modules.txt`, `console-x86-vulkan_probe.png` | Virtio-GPU Venus (NVIDIA GeForce RTX 5090), 57.43 fps, readback ok, 7 graphics modules all in system paths, 0 app-local; console: window found, animating |
| 2 | dxdiag reports hardware acceleration on our driver | **pass** | `dxdiag/dxdiag.json` | hardware_acceleration=True, card 'Hardsoft VirtIO GPU 3D controller', D3D Enabled, DDraw Enabled, WDDM 2.0 |
| 2 (extra) | dxdiag: Direct3D acceleration on our full WDDM device | **pass** | `dxdiag/dxdiag.json` | D3D Status Enabled, Full Device (POST), WDDM 2.0 |
<!-- /e2e:results -->

## Stretch: unmodified third-party applications

`tests/e2e/run.sh --apps` (that is, `tests/e2e/apps.sh`) also runs real
applications, unmodified, from official downloads pinned by URL and sha256 in
`tests/e2e/PINS`. Each runs in the user's session through `Run-Probe.ps1`
(`-Raw`) for 30 to 90 s and is then closed. `vmlab vm screenshot` captures
the console every 4 s, and the capture from two thirds of the way through is
kept. These results do not count towards the exit status. Evidence:
[docs/evidence/acceptance/<template>/apps/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/acceptance/).

The prerequisites are installed system-wide, as a game installer would do
it: the DirectX End-User Runtime from the SDK's own `Redist\DXSETUP.exe
/silent` (D3DX 9/10/11 and D3DCompiler_43), and the Visual C++ 2010 SP1
runtime (`MSVCR100.dll`) that the SDK samples import (`apps/dxsetup.txt`).

The 0.1.1-alpha-rc1 runs, on five labs. Each cell names the capture in
[docs/evidence/acceptance/<run>/apps/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/acceptance/). The rc2 runs on AMD1 repeated the
AMD1 cells on both templates with the same result: every application
renders, ran until it was closed, and loaded no app-local graphics DLL.
The rc3 runs repeated the NVIDIA cells and windows-11 on AMD1 with
Resizable BAR on. Every application rendered and loaded no app-local graphics DLL. On
windows-11 Nvidia2 and AMD1, Blender's kept capture shows its window
before it had drawn, and a rerun in the Nvidia2 lab rendered the full UI
([rc3](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/rc3/README.md#blenders-kept-capture-is-black-on-windows-11-nvidia2-and-amd1)).
The captures in `windows-11/apps/` and the `*-nvidia*` directories are
now rc3's, and `windows-10/apps/` is rc2's.

| Application (pinned in `tests/e2e/PINS`) | API | windows-11: AMD1, Nvidia1, Nvidia2 | windows-10: AMD1, Nvidia1 | Modules |
| --- | --- | --- | --- | --- |
| DirectX SDK June 2010 `BasicHLSL.exe`, x64 | D3D9 | **renders** on all three: the animated, lit, textured "tiny" model with DXUT's UI; the overlay reads `HAL (hw vp): Hardsoft VirtIO GPU 3D controller` (`console-d3d9-x64-BasicHLSL.png`) | **renders** on both, same | `d3d9.dll`, `viogpu_d3d10.dll`, D3DX 9 from System32; 0 app-local |
| Same, x86 | D3D9 | **renders** on all three, same, through SysWOW64 (`console-d3d9-x86-BasicHLSL.png`) | **renders** on both | as above, SysWOW64 |
| DirectX SDK `ParallaxOcclusionMapping.exe`, x64 | D3D9 | **renders** on all three: the textured, parallax-occlusion-mapped stone disc, HAL on our device (`console-d3d9-x64-ParallaxOcclusionMapping.png`) | **renders** on both | as above |
| DirectX SDK `BasicHLSL11.exe`, x64 | D3D11 | **renders** on all three: the lit, textured "tiny" model in an 800×600 window; the overlay reads `HARDWARE: Hardsoft VirtIO GPU 3D controller` (`console-d3d11-x64-BasicHLSL11.png`) | **renders** on both | `d3d11.dll`, `dxgi.dll`, `viogpu_d3d10.dll`, D3DX 11 from System32; 0 app-local |
| DirectX SDK `SubD11.exe`, x64 | D3D11 | **renders** on all three: the tessellated character with the green wire overlay, on `HARDWARE: Hardsoft VirtIO GPU 3D controller` (`console-d3d11-x64-SubD11.png`) | **renders** on both | as above |
| Blender 4.2.3 LTS, `blender.exe --factory-startup` | OpenGL 4.3 | **renders**, opaque, on all three: the complete UI, 3D viewport, grid and gizmos (`console-opengl-x64-Blender.png`) | **renders, opaque**, on both, with the splash still in front; the wallpaper no longer shows through (`patches/mesa/wgl-present-windows-opaque.patch`) | `opengl32.dll`, `viogpu_wgl.dll` with `libvulkan_virtio.dll`; app-local only Blender's own non-graphics DLLs (`python311.dll`, `python3.dll`, `blender_cpu_check.dll`, `cycles_kernel_oneapi_aot.dll`) |

On NVIDIA every application ran to its time limit. The host kernel logged
no Xid during any rc run on Nvidia1 or Nvidia2
([docs/evidence/rc/host-kernel-nvrm.txt](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/rc/host-kernel-nvrm.txt)). Before 0.1.1, `BasicHLSL` hung
the guest with Xid 69 on both cards ([`xid69.md`](xid69.md)), so the NVIDIA
runs had no application results.

Every application ran as the logged-on user in session 1 and was still
running when it was closed (`timed_out: true` in `<app>.run.json`). The
module lists are in `<app>.modules.txt`.

**Both D3D11 samples now render in the e2e run on both templates.** The
earlier acceptance runs ([b3e1798](https://github.com/VMLabDev/vmlab-win3d/commit/b3e179837e76ff7ebf013ce90e60a586af5138a2), [cb4d53c](https://github.com/VMLabDev/vmlab-win3d/commit/cb4d53c67556805416790be6cf6246781a003d29)) recorded both as failures.
`docs/d3d11-apps.md` found two causes:

- SubD11 hit three UMD bugs, fixed by `patches/mesa/d3d10umd-hull-shader-vprim-and-indexed-patch-io.patch` to `0017`.
- The June 2010 DXUT places BasicHLSL11's window off the screen on Windows
  8 and later unless the SDK's `D3D11Ref.dll` is present. That is an
  application bug ([known limits](known-limits.md)). `tests/e2e/apps.sh`
  installs `D3D11Ref.dll` from the pinned SDK's developer runtime into
  System32 and SysWOW64, as the SDK installer would (`apps/dxsetup.txt`
  gives the hashes).

DXUT loads `D3D11Ref.dll` only while it enumerates devices. It is not in
either sample's module list, and both overlays name the HARDWARE device.

**Not attempted here: D3D12.** (Done later, with clang-cl and the MSVC headers:
[d3d12app.md](d3d12app.md).) The candidate was Microsoft's
DirectX-Graphics-Samples `D3D12HelloTriangle` (MIT), built from source with
the project toolchain, llvm-mingw. It does not compile unmodified with that
toolchain:

- The sample calls `ID3D12DescriptorHeap::GetCPUDescriptorHandleForHeapStart()`
  in the MSVC return-value form. DirectX-Headers deliberately declares the
  MinGW-ABI form, with a `RetVal` out-parameter, for non-MSVC Windows
  compilers.
- It uses `Microsoft::WRL::Wrappers::FileHandle`, which mingw's WRL does not
  provide.
- It uses MSVC's `_uuidof`.

Building it unmodified needs MSVC or clang-cl with the Windows SDK. Patching
the source would defeat the purpose, so this stretch item is skipped.

## Adapter suite

`tests/e2e/run.sh --apps` ends with `tests/e2e/adapter.sh`, after the
stretch applications. The suite asks one question the Done checks do not:
does the device behave like a real video card? It checks the properties
that a test can check, with the property numbers of the design it comes from
(docs/plans/win3d-real-adapter/DESIGN.md at the root of the vmlab
repository, section 3). A property that is missing today has a check that fails
today: the tables below are a baseline, and a check that changes from pass to
fail in a later run is a regression. The suite does not count towards the
exit status of `run.sh`.

```sh
tests/e2e/run.sh --template windows-11 --host-gpu Nvidia2 --apps      # the whole run
tests/e2e/adapter.sh <lab-dir> win <evidence-dir> [check...]          # against a lab kept with --keep
```

Every check prints `pass`, `fail` or `not-run`. `not-run` means that nothing
was measured, and the note says why: the check was not selected, its program
did not start, or what it needs was missing (no WebGL on the adapter, a
download that failed, a guest that stopped answering). The probes only
measure. The limits are in `tests/e2e/adapter/checks/`. The judge writes
`adapter/adapter.json` and `adapter/adapter.txt` in the evidence directory,
next to each probe's JSON.

| Property | Check | What runs | Pass when |
| --- | --- | --- | --- |
| P1, a hardware pointer | `pointer` | `pointer_probe.exe` fills the display with one colour. The host moves the pointer to two places with `vmlab vm mouse-move` and takes a console capture at each (`tests/e2e/adapter/pointerframe.py`). | The capture is the flat colour where the pointer is, while Windows reports a visible pointer at that place. |
| P1 | `pointer-shapes` | `tests/e2e/pointer.sh`, the check of the hardware pointer ([pointer.md](pointer.md#check)): arrow, I-beam, resize and hidden, watched on vmlab-hv's frame protocol, on RFB, in captures and in a recording. Its evidence is in `pointer/`. | Its five checks pass. |
| P2, a vblank of the display | `vblank-interval` | `pace_probe.exe` times 10 s of `IDXGIOutput::WaitForVBlank`. | The mean interval is within 2 percent of the mode's refresh period, and the 1st and 99th percentile within half a period of it. |
| P2 | `present-rate` | The same probe times 10 s of `Present(1)` on a flip-model swap chain. | The frame rate is within 5 percent of the refresh rate. |
| P2 | `flip-timestamps` | DXGI frame statistics over those presents. | `SyncRefreshCount` over `SyncQPCTime` is within 2 percent of the refresh rate. |
| P2 | `scan-line` | One second of `D3DKMTGetScanLine`. | It answers more than one line, and "in vertical blank" at least once. |
| P3 to P5, a flip shows a finished frame | `tear-flip` | `tear_probe.exe --mode flip`: every frame of its window is one colour of eight, drawn top to bottom in 48 strips by a slow pixel shader, without a clear. It animates for 3 s and holds for 3 s, for 40 s. `tests/e2e/adapter/rfbgrab.py` reads the window from the console's RFB socket about 30 times a second. | No console frame has rows of two colours. At least 100 frames showed the window. |
| P3 to P5 | `tear-blt` | The same with `--mode blt`: a blt-model swap chain, the present of the Blt kind that windowed Direct3D 9, WPF and DirectDraw make. | As above. |
| P7, Desktop Duplication | `duplication` | `dupl_probe.exe` takes eight frames with `AcquireNextFrame` while a window of its own changes colour. | Eight frames, each at least half lit, each with the window in the colour it had. |
| P7 | `shared-texture` | `share_probe.exe` shares a BGRA render target between two Direct3D 11 devices, by legacy handle and with a keyed mutex, at the size of the desktop (the shape the driver takes for a duplication surface, [present.md](present.md#desktop-duplication)) and at 256×256. Each device fills it and the other reads it. | In all four cases every pixel read is the colour written, in both directions. |
| P10, correct rendering | `d2d` | `d2d_probe.exe` fills rounded rectangles, ellipses and path geometries with Direct2D three times, each on a new device, and once on WARP. | No pixel WARP filled is missing, none it left white is drawn, and the three pictures are the same. The device must be feature level 11.1. |
| P10 | `d2d-tir` | `tests/e2e/tir.sh`, the check of the coverage patch ([d2d-coverage.md](d2d-coverage.md)): `tir_probe.exe`, x64 and x86, draws what Direct2D draws to antialias a shape, on the adapter and on WARP. Its evidence is in `tir/`. | Both probes find no pixel that differs from WARP or between runs, and the driver logged one pass per sample for the x64 probe. |
| P10 | `svb-chrome`, `svb-edge` | `tests/e2e/adapter/pages/svb.html`, the page of GitHub #233: a quad from a vertex buffer of each usage hint. | Each of the three lines reads `255,0,0,255`, with WebGL on the adapter. |
| P8, a driver for every API | `venc-chrome`, `venc-edge` | `tests/e2e/adapter/pages/venc.html`, the page of GitHub #238: a hardware video encoder query, which makes a Chromium GPU process load `d3d12.dll`, then WebGL every 4 s for 20 s. The browsers run with `--enable-features=D3D12VideoEncodeAccelerator,D3D12VideoDecoder`. | WebGL stays on the adapter in all five tries, and the profile has no crash dump. |
| P8 | `chromium` | `tests/e2e/chromium.sh`, the check of the native `d3d12.dll` preload ([chromium.md](chromium.md)): the same page in both browsers while the GPU process's modules are recorded, the same again with the preload off, and the package's `d3d12_probe.exe`. It restarts the VM twice. Its evidence is in `chromium/`. | In both browsers WebGL stays on the adapter, there is no crash dump, one GPU process ran, and the `d3d12.dll` it loaded is the native copy. With the preload off the GPU process ends itself in both. The native probe still gets the bridge. |
| P10 | `wine-<program>` | Wine's Direct3D 9, 10 and 11 tests, one row per test program. | The program reaches its summary line with 0 failures. |

Chrome is Chrome for Testing 158, pinned in `tests/e2e/PINS`. Edge is the
one the template has. A page writes its result to the browser's console, and
`tests/e2e/adapter/guest/Run-Page.ps1` reads it from the browser's log file.

The Wine tests are the nine programs that `tests/e2e/adapter/build.sh`
builds from the Wine 11.0 source pinned in `tests/e2e/PINS`: `d3d9ex`,
`device`, `stateblock` and `visual` of d3d9, `device` and `effect` of d3d10,
and the single test files of d3d10core, d3d10_1 and d3d11. WineHQ no longer
publishes a prebuilt `winetest`, so the suite builds each file as its own
program with the toolchain image (`-DSTANDALONE`), against the mingw-w64
headers. Each runs in the user's session for at most 900 s
(`ADAPTER_WINE_TIMEOUT`). A program that does not reach its summary line in
that time counts as a failure, and so does one that ends on an unhandled
exception after it printed a summary. The lines that say what failed are
kept as `adapter/wine-<program>.failures.txt`. The tests run last, because
they change display modes and reset devices. When a program ends the VM, its
row says so with the last line of `vmlab-hv.log`, and the suite starts the
VM again for the next program, at most three times a run.

Properties the suite does not check: P6 (it has no test of where a Blt
present is copied), P9 (it needs an Intel CPU host with an NVIDIA GPU), P11
to P14.

### Baseline

The runs of 2026-11-20 on the package that has the hardware pointer
([pointer.md](pointer.md)) and the coverage patch
([d2d-coverage.md](d2d-coverage.md)): the `windows-11` template with the
0.1.3 candidate package (driver `100.6.101.58106`, the November base) on
vmlab-hv, on Nvidia2 at commit `0216e6d0b5fa` and on AMD1 at commit
`4b8e55853639`, each with `tests/e2e/run.sh --template windows-11 --host-gpu
<gpu> --apps`. Between the two commits only `tests/e2e/adapter.sh` changed
(B6). Both runs passed 12 of 12 Done checks first, and the host kernel logged
no Xid. `tests/e2e/report.py` writes the two tables from each run's
`adapter/adapter.json`.

An earlier baseline, at commit `41e2e2fa5b49` on the package without those
two changes (driver `100.6.101.58105`), had 11 checks pass and 11 fail on
both GPUs. `pointer` failed there: Windows drew the pointer into the frame,
144 pointer pixels at each place (GitHub #235). The checks `pointer-shapes`
and `d2d-tir` were not in the suite.

What the baseline says:

- B1. Both GPUs give the same result: the same 14 checks pass and the same
  10 fail.
- B2. Against the earlier baseline one check changed: `pointer` passes. The
  two new checks, `pointer-shapes` and `d2d-tir`, pass. No check changed
  from pass to fail.
- B3. Three failures are the gaps the design names. `scan-line`:
  `D3DKMTGetScanLine` answers line 0 and never "in vertical blank", because
  vsync is a guest timer. `duplication`: every `AcquireNextFrame` answers
  `DXGI_ERROR_ACCESS_LOST` (0x887a0026, GitHub #237). `venc-chrome` and
  `venc-edge`: the GPU process ends itself over the Direct3D 12 bridge and
  leaves crash dumps (GitHub #238). How far a browser gets differs from run
  to run. Chrome lost WebGL in every run, before the page's first context
  or after the query. Edge kept WebGL with two crash dumps on Nvidia2, lost
  it before the first context in the AMD1 run of the table, and lost it
  after the query in an AMD1 run 50 minutes before. Edge is 158.0.4370.33
  here and was 157.0.4341.60 in the earlier baseline: the guest's Edge
  updates itself, so the Edge rows are not from a pinned browser.
- B4. The vblank interval, the present rate and the flip timestamps are
  within their limits. One present in a hundred takes two refresh periods
  (p99 33 ms). The present rate on AMD1 is 57.9 frames per second, and it
  was 57.5 in the AMD1 run that shared the host with the Nvidia2 run. The
  limit is 57.0, so this row can fail on a loaded host.
- B5. Three of Wine's nine test programs pass: `d3d10_1` and both d3d10
  programs. `d3d10core` and `d3d11` end on an access violation after calls
  begin to answer `DXGI_ERROR_DEVICE_REMOVED` (0x887a0005). The d3d9
  programs run to their end with failures: 11 in `d3d9ex`, 169 in
  `stateblock`, and 343 (AMD1) or 345 (Nvidia2) of about 206,000 checks in
  `visual`. The [D3D9 failure groups](#d3d9-failure-groups) trace the
  failed assertions to the Wine 11.0 source.
  B10 says which calls removed the device, and the fix.
- B6. Wine's d3d9 `device` program ends the hypervisor, on both GPUs, 13 to
  26 s in: vmlab-hv exits with "Too many open files" after a run of `GPU
  dma-buf import failed: 0x3003` lines for 800x600 scanouts. vmlab-hv writes the digit 4 in its log messages as
  `[REDACTED]`, so the row reads `os error 2[REDACTED]` for os error 24.
  This is a host fault that an unprivileged guest program can cause. The
  suite starts the VM again and goes on. `vmlab-hv.log` outlives a run of
  the lab, and the first AMD1 run of this baseline quoted the exit line of
  an older run in this row ("TPM shutdown ... Broken pipe"). The suite now
  waits for an exit line that is new, and the AMD1 table is the run after
  that fix. Its other rows gave the same results as the first run, but for
  the difference in `venc-edge` that B3 names.
- B7. Three checks pass that have never been seen to fail on this host, so
  the baseline does not prove that they detect their fault. `tear-flip` and
  `tear-blt`: no torn frame in about 1,260 console frames each. GitHub #231
  saw 0 or 1 torn frames in 2,000 without host load, and the suite does not
  load the host. `d2d`: no hole on either GPU, in the earlier baseline
  without the coverage patch either. Its shapes do not reach the path of
  GitHub #236 ([d2d-coverage.md](d2d-coverage.md)). `d2d-tir` is the check
  of that path: its probe found pixels that differ on both GPUs without the
  patch. The failing checks above, and `pointer` in the earlier baseline,
  show that their probes do detect.
- B8. After the baseline: with driver `100.6.101.58107` (`patches/kmd/kmd-present-blt-into-mappable-blob.patch`
  and `patches/mesa/yttrium-desktop-duplication-surface-as-kernel-written-display-image.patch`) `duplication` passes on both GPUs, eight lit
  frames of eight, and the new check `shared-texture` passes
  ([present.md](present.md#desktop-duplication)). Those runs selected
  `duplication`, `shared-texture` and `tear-blt` only.
- B9. The tables below are not the baseline's. They are the runs of
  2026-11-20 at commit `d88304f2eab7`, with the Mesa series 0001 to 0096
  (driver `100.6.101.58107`, the Chromium preload of
  [chromium.md](chromium.md) added), each with `tests/e2e/run.sh --template
  windows-11 --host-gpu <gpu> --apps`. Both passed 12 of 12 Done checks
  first, and the host kernel logged no Xid. Against the baseline,
  `duplication`, `venc-chrome` and `venc-edge` changed from fail to pass on
  both GPUs, and the new checks `shared-texture` and `chromium` pass.
  `venc-chrome` and `venc-edge` kept WebGL in 5 of 5 tries with no crash
  dump. Nvidia2 has 19 checks pass and 7 fail. AMD1 has 18 pass and 8 fail:
  `present-rate` changed to fail at 56.54 frames per second against the
  limit 57.0, the loaded-host case of B4 (a run of the AMD1 suite at commit
  `f8b02e71` an hour before had 56.52). The other failures are the
  baseline's, `wine-d3d9_device` again ending the VM as B6 says.
- B10. The device removal of B5 (GitHub #248). Single-threaded reruns
  (`WINETEST_NO_MT_D3D=1`) with `C:\ProgramData\Yttrium\yttrium-errors.log`
  name each UMD call that set an error the runtime does not allow, which
  removes the device. The access violation is the test reading through a
  pointer that a failed call left NULL. `patches/mesa/d3d10umd-keep-the-device-through-wine-d3d10core-and-d3d11-tests.patch` changes these:
  - `GetResourceLayout` (WDDM 2.0) set `E_INVALIDARG`. The runtime asks for
    it for each DEFAULT texture created with CPU access flags
    (`test_resource_access`, the first `0x887a0005` lines of both programs).
    It answers an undefined layout now.
  - The Yttrium vertex input had no Vulkan format for `R11G11B10_FLOAT`,
    so the draw was dropped and the next draw set `E_FAIL`
    (`test_vertex_formats`). It is `B10G11R11_UFLOAT_PACK32` now.
  - A pixel shader that needs more than 32 combined texture slots set
    `E_FAIL` at its creation (`test_high_resource_count`), and the test then
    released the NULL shader. It is created as a shader that writes zero
    ([known-limits.md](known-limits.md#direct3d-10-and-11-at-most-32-texture-and-sampler-pairs-in-one-shader)).

  The runs of 2026-11-21 at commit `022cd405`, each a fresh lab with
  `tests/e2e/run.sh --template windows-11 --host-gpu <gpu> --keep` and
  vmlab-hv and vmlab built from `b09d50f0`, the base of that commit (12 of 12 Done checks pass on both GPUs, no
  Xid): `d3d10core` executes 195908 checks with 0 failures on AMD1
  (single-threaded) and 195912 with 0 failures on Nvidia2 (the suite's
  run). `d3d11` reaches its summary on both, 476333 (AMD1) and 476337
  (Nvidia2) checks with 14 failures, exit code 14, no unhandled exception,
  and no `0x887a0005` line. On Nvidia2 `d3d10_1`, `d3d10_device` and
  `d3d10_effect` still pass with 0 failures. The 14 failures: 11 NV12
  textures `test_nv12` cannot create (`E_OUTOFMEMORY`), the two checks of
  `test_high_resource_count`, and `B8G8R8X8_UNORM` in `test_vertex_formats`,
  whose draw is dropped because one input element has no Vulkan vertex
  format. That dropped draw and one more in `test_index_buffer_offset`
  (a point-list draw with no index buffer bound, whose indices Yttrium
  does not have on the CPU) still mark a device as removed in the UMD log,
  but the tests release those devices without another call that reports
  it. Before the fix, with the vmlab-hv bundle of 2026-11-19, `d3d10core`
  also ended the VM after 17 s with the "Too many open files" exit of B6.

See the [acceptance notes](acceptance/) for card-specific results and follow-ups.

<!-- e2e:adapter -->
### windows-11

Host GPU: AMD1, /dev/dri/renderD130 0000:72:00.0 VGA compatible controller: Advanced Micro Devices, Inc. [AMD/ATI] Granite Ridge [Radeon 610M] (rev c1). Run 2026-11-20T12:31:25+10:00 at commit `d88304f2eab7`, template `x86_64/windows-11@26100.1742.11`. **18 pass, 8 fail, 0 not-run.**

| Property | Check | Result | Measured |
| --- | --- | --- | --- |
| P1 | `pointer`: the pointer is not part of the frame | **pass** | the capture is the flat fill colour at both places while Windows reports a visible pointer there |
| P1 | `pointer-shapes`: the pointer's shapes, positions and hiding reach the host | **pass** | shapes pass, rfb pass, outline pass, frame pass, recording pass |
| P2 | `vblank-interval`: the vblank interval is the refresh period | **pass** | mean 16.661 ms, p01 15.87, p99 17.18, max 19.44 over 600 intervals; the 60 Hz mode is 16.667 ms (limits: mean within 2 percent, p01 and p99 within half a period) |
| P2 | `present-rate`: a vsynced present runs at the refresh rate | **fail** | 56.54 frames per second with sync interval 1 on a 60 Hz mode (limit 5 percent); interval p01 14.92 ms, p99 34.07 ms |
| P2 | `flip-timestamps`: frame statistics count the refreshes | **pass** | 599 refreshes in 9984 ms by SyncRefreshCount and SyncQPCTime: 60.00 Hz on a 60 Hz mode (limit 2 percent); 563 presents |
| P2 | `scan-line`: the scan line moves and reaches vertical blank | **fail** | 1 different lines (highest 0) and 0 answers in vertical blank in 19905 samples over one second |
| P3-P5 | `tear-flip`: no torn frame on the console, flip-model presents | **pass** | 0 torn of 1260 console frames (0 of them unchanged from the frame before), 34.1 grabs per second; the probe presented 1286 frames at 61.3 per second |
| P3-P5 | `tear-blt`: no torn frame on the console, presents of the Blt kind | **pass** | 0 torn of 1258 console frames (0 of them unchanged from the frame before), 34.4 grabs per second; the probe presented 1264 frames at 60.1 per second |
| P7 | `duplication`: Desktop Duplication delivers lit frames | **pass** | 8 frames, each with the window in the colour it had; at least 100.0% of a frame is lit |
| P7 | `shared-texture`: A shared BGRA render target works between two devices | **pass** | 4 cases, each read whole in both directions: 1024x768 and 256x256, by legacy handle and with a keyed mutex |
| P10 | `d2d`: Direct2D fills antialiased shapes without gaps | **pass** | 0 holes and 0 spills against WARP, 0 pixels differ between 3 runs, 0 edge pixels off by more than 64, 176553 filled pixels |
| P10 | `d2d-tir`: Direct2D antialiasing draws the coverage that WARP draws | **pass** | pass x64: 0 of 93467 covered pixels differ from WARP, 0 differ between 3 runs, 0 coverage bits lost, 0 extra; driver: passes (3 pipelines); pass x86: 0 of 93467 covered pixels differ from WARP, 0 differ between 3 runs, 0 coverage bits lost, 0 extra; driver: no line (0 pipelines); judged by the picture alone |
| P10 | `svb-chrome`: WebGL draws from a STATIC_DRAW vertex buffer in Chrome | **pass** | Chrome 158.0.8127.36: STATIC_DRAW 255,0,0,255, DYNAMIC_DRAW 255,0,0,255, STREAM_DRAW 255,0,0,255; ANGLE (VirtIO, Yttrium VirtIO GPU 3D controller (0x00001050) Direct3D11 vs_5_0 ps_5_0, D3D11) |
| P8 | `venc-chrome`: Chrome's GPU process survives the video encoder query | **pass** | Chrome 158.0.8127.36: WebGL on the adapter in 5 of 5 tries over 20 s after the query, 0 crash dumps; encoder supported: false; WebGPU no adapter |
| P10 | `svb-edge`: WebGL draws from a STATIC_DRAW vertex buffer in Edge | **pass** | Edge 158.0.4370.33: STATIC_DRAW 255,0,0,255, DYNAMIC_DRAW 255,0,0,255, STREAM_DRAW 255,0,0,255; ANGLE (VirtIO, Yttrium VirtIO GPU 3D controller (0x00001050) Direct3D11 vs_5_0 ps_5_0, D3D11) |
| P8 | `venc-edge`: Edge's GPU process survives the video encoder query | **pass** | Edge 158.0.4370.33: WebGL on the adapter in 5 of 5 tries over 20 s after the query, 0 crash dumps; encoder supported: false; WebGPU no adapter |
| P8 | `chromium`: a Chromium GPU process loads the native d3d12.dll and ends itself without it; a native program keeps the bridge | **pass** | chrome pass, chrome-control pass, edge pass, edge-control pass, native pass |
| P10 | `wine-d3d10_1_d3d10_1`: Wine's d3d10_1 test d3d10_1 passes | **pass** | 580 tests executed, 0 failures, 0 todo, 5 skipped in 4 s |
| P10 | `wine-d3d10core_d3d10core`: Wine's d3d10core test d3d10core passes | **fail** | ended on an unhandled exception (exit code 0xc0000005) after 16 s; 4 failed checks printed before it |
| P10 | `wine-d3d10_device`: Wine's d3d10 test device passes | **pass** | 206 tests executed, 0 failures, 0 todo, 0 skipped in 3 s |
| P10 | `wine-d3d10_effect`: Wine's d3d10 test effect passes | **pass** | 5041 tests executed, 0 failures, 0 todo, 0 skipped in 4 s |
| P10 | `wine-d3d11_d3d11`: Wine's d3d11 test d3d11 passes | **fail** | ended on an unhandled exception (exit code 0xc0000005) after 30 s; 93 failed checks printed before it |
| P10 | `wine-d3d9_d3d9ex`: Wine's d3d9 test d3d9ex passes | **fail** | 2379 tests executed, 11 failures, 0 todo, 41 skipped in 30 s |
| P10 | `wine-d3d9_device`: Wine's d3d9 test device passes | **fail** | the VM stopped 14 s into the program: process exit: RuntimeError; exit_code=1; error_chain=["control accept loop: Too many open files (os error 2[REDACTED])"] |
| P10 | `wine-d3d9_stateblock`: Wine's d3d9 test stateblock passes | **fail** | 12268 tests executed, 169 failures, 0 todo, 0 skipped in 4 s |
| P10 | `wine-d3d9_visual`: Wine's d3d9 test visual passes | **fail** | 206068 tests executed, 343 failures, 0 todo, 31 skipped in 68 s |

### windows-11-nvidia2

Host GPU: Nvidia2, /dev/dri/renderD129 0000:05:00.0 VGA compatible controller: NVIDIA Corporation GB202 [GeForce RTX 5090] (rev a1). Run 2026-11-20T12:45:13+10:00 at commit `d88304f2eab7`, template `x86_64/windows-11@26100.1742.11`. **19 pass, 7 fail, 0 not-run.**

| Property | Check | Result | Measured |
| --- | --- | --- | --- |
| P1 | `pointer`: the pointer is not part of the frame | **pass** | the capture is the flat fill colour at both places while Windows reports a visible pointer there |
| P1 | `pointer-shapes`: the pointer's shapes, positions and hiding reach the host | **pass** | shapes pass, rfb pass, outline pass, frame pass, recording pass |
| P2 | `vblank-interval`: the vblank interval is the refresh period | **pass** | mean 16.662 ms, p01 15.44, p99 17.66, max 19.34 over 600 intervals; the 60 Hz mode is 16.667 ms (limits: mean within 2 percent, p01 and p99 within half a period) |
| P2 | `present-rate`: a vsynced present runs at the refresh rate | **pass** | 58.32 frames per second with sync interval 1 on a 60 Hz mode (limit 5 percent); interval p01 14.95 ms, p99 33.07 ms |
| P2 | `flip-timestamps`: frame statistics count the refreshes | **pass** | 596 refreshes in 9935 ms by SyncRefreshCount and SyncQPCTime: 59.99 Hz on a 60 Hz mode (limit 2 percent); 578 presents |
| P2 | `scan-line`: the scan line moves and reaches vertical blank | **fail** | 1 different lines (highest 0) and 0 answers in vertical blank in 19942 samples over one second |
| P3-P5 | `tear-flip`: no torn frame on the console, flip-model presents | **pass** | 0 torn of 1259 console frames (0 of them unchanged from the frame before), 33.0 grabs per second; the probe presented 1270 frames at 60.6 per second |
| P3-P5 | `tear-blt`: no torn frame on the console, presents of the Blt kind | **pass** | 0 torn of 1262 console frames (0 of them unchanged from the frame before), 34.3 grabs per second; the probe presented 1247 frames at 59.3 per second |
| P7 | `duplication`: Desktop Duplication delivers lit frames | **pass** | 8 frames, each with the window in the colour it had; at least 100.0% of a frame is lit |
| P7 | `shared-texture`: A shared BGRA render target works between two devices | **pass** | 4 cases, each read whole in both directions: 1024x768 and 256x256, by legacy handle and with a keyed mutex |
| P10 | `d2d`: Direct2D fills antialiased shapes without gaps | **pass** | 0 holes and 0 spills against WARP, 0 pixels differ between 3 runs, 0 edge pixels off by more than 64, 176553 filled pixels |
| P10 | `d2d-tir`: Direct2D antialiasing draws the coverage that WARP draws | **pass** | pass x64: 0 of 93467 covered pixels differ from WARP, 0 differ between 3 runs, 0 coverage bits lost, 0 extra; driver: passes (3 pipelines); pass x86: 0 of 93467 covered pixels differ from WARP, 0 differ between 3 runs, 0 coverage bits lost, 0 extra; driver: no line (0 pipelines); judged by the picture alone |
| P10 | `svb-chrome`: WebGL draws from a STATIC_DRAW vertex buffer in Chrome | **pass** | Chrome 158.0.8127.36: STATIC_DRAW 255,0,0,255, DYNAMIC_DRAW 255,0,0,255, STREAM_DRAW 255,0,0,255; ANGLE (VirtIO, Yttrium VirtIO GPU 3D controller (0x00001050) Direct3D11 vs_5_0 ps_5_0, D3D11) |
| P8 | `venc-chrome`: Chrome's GPU process survives the video encoder query | **pass** | Chrome 158.0.8127.36: WebGL on the adapter in 5 of 5 tries over 20 s after the query, 0 crash dumps; encoder supported: false; WebGPU no adapter |
| P10 | `svb-edge`: WebGL draws from a STATIC_DRAW vertex buffer in Edge | **pass** | Edge 158.0.4370.33: STATIC_DRAW 255,0,0,255, DYNAMIC_DRAW 255,0,0,255, STREAM_DRAW 255,0,0,255; ANGLE (VirtIO, Yttrium VirtIO GPU 3D controller (0x00001050) Direct3D11 vs_5_0 ps_5_0, D3D11) |
| P8 | `venc-edge`: Edge's GPU process survives the video encoder query | **pass** | Edge 158.0.4370.33: WebGL on the adapter in 5 of 5 tries over 20 s after the query, 0 crash dumps; encoder supported: false; WebGPU no adapter |
| P8 | `chromium`: a Chromium GPU process loads the native d3d12.dll and ends itself without it; a native program keeps the bridge | **pass** | chrome pass, chrome-control pass, edge pass, edge-control pass, native pass |
| P10 | `wine-d3d10_1_d3d10_1`: Wine's d3d10_1 test d3d10_1 passes | **pass** | 580 tests executed, 0 failures, 0 todo, 5 skipped in 78 s |
| P10 | `wine-d3d10core_d3d10core`: Wine's d3d10core test d3d10core passes | **fail** | ended on an unhandled exception (exit code 0xc0000005) after 49 s; 4 failed checks printed before it |
| P10 | `wine-d3d10_device`: Wine's d3d10 test device passes | **pass** | 206 tests executed, 0 failures, 0 todo, 0 skipped in 3 s |
| P10 | `wine-d3d10_effect`: Wine's d3d10 test effect passes | **pass** | 5041 tests executed, 0 failures, 0 todo, 0 skipped in 16 s |
| P10 | `wine-d3d11_d3d11`: Wine's d3d11 test d3d11 passes | **fail** | ended on an unhandled exception (exit code 0xc0000005) after 74 s; 93 failed checks printed before it |
| P10 | `wine-d3d9_d3d9ex`: Wine's d3d9 test d3d9ex passes | **fail** | 2377 tests executed, 15 failures, 0 todo, 41 skipped in 86 s |
| P10 | `wine-d3d9_device`: Wine's d3d9 test device passes | **fail** | the VM stopped 24 s into the program: process exit: RuntimeError; exit_code=1; error_chain=["control accept loop: Too many open files (os error 2[REDACTED])"] |
| P10 | `wine-d3d9_stateblock`: Wine's d3d9 test stateblock passes | **fail** | 12268 tests executed, 169 failures, 0 todo, 0 skipped in 5 s |
| P10 | `wine-d3d9_visual`: Wine's d3d9 test visual passes | **fail** | 206068 tests executed, 345 failures, 0 todo, 31 skipped in 164 s |
<!-- /e2e:adapter -->

### D3D9 failure groups

Card #253 uses the `adapter/wine-<program>.failures.txt` logs for the
Windows 11 tables above. Counts below are failed assertions, including
readback failures that follow a rejected draw. The Wine source locations
identify the failing behavior. Several rendering groups still need a
driver-level diagnosis.

`stateblock.c:1233` checks the result of setting each render state.
The driver rejected nonzero values for `WRAP0` through `WRAP15`,
`D3DZB_USEW` for `ZENABLE`, and wireframe for `FILLMODE`. The same
rejections can propagate through `Apply` at line 297. These two assertions
account for 162 and seven failures respectively on each GPU. The
[coordinate-wrap patch](../patches/mesa/d3d9-record-coordinate-wrap-render-states.patch)
fixes the largest setter group by retaining the wrap flags through state
block replay. Cylindrical interpolation still needs implementation.

On helios with AMD1 and Windows 11 template `26100.1742.11`, the patched
package ran the same 12,268 `stateblock` checks. Failures fell from 169 to
25. The patch removed 144 setter failures, leaving 18 setter failures and
seven replay failures. `d3d9ex` still reported 11 failures in 2,379 checks.
The standalone `visual` run on AMD1 reported the same 343 failures in
206,068 checks. The test session used a soft open-file limit of 65,536
after an earlier attempt exhausted the default limit of 1,024.

The [unbound vertex stream patch](../patches/mesa/d3d9-default-inputs-for-unbound-streams.patch)
uses the existing zero input when a vertex declaration names a stream
with no bound source. Wine's `visual` ran 206,068 checks with 31 skips on
each GPU on Windows 11 template `26100.1742.11#win3d`. Failures fell from
343 to 303 on AMD1 and from 345 to 305 on Nvidia2. All 14 fixed-function
draw and color readback failures at lines 25308 and 25313 and all 12
missing attribute draw and readback failures at lines 27740 and 27745
cleared on both GPUs. The 14 size-one point failures at lines 11460
and 11498 also cleared. No new assertions failed. The Win3D end-to-end
run on the prepared AMD1 clone passed all 12 Done checks.

#### Table fog (card #335)

Table fog selects screen Z when the runtime reports `(1, 1)` through
`UpdateWInfo`. Equal non-unit endpoints still select W fog. The pixel
shader recovers W from the rasterizer's reciprocal clip W for
pre-transformed, fixed-function and programmable vertices.
[Mesa patch](../patches/mesa/d3d9-select-table-fog-from-w-info.patch).

On helios, Wine 11.0's `d3d9_visual` ran 206,068 checks with 31 skips per
GPU on Windows 11 template `26100.1742.11` using the signed package from
`ab2afacc`. The targeted assertions are at `visual.c:28481` with table fog
enabled and `ortho_fog = 0`.

| D3D9 visual failures | AMD1 before → after | Nvidia2 before → after |
| --- | --- | --- |
| Pre-transformed vertices, reciprocal RHW (`vs_mode = 0`) | 48 → 0 | 48 → 0 |
| Fixed-function transformed W (`vs_mode = 1`) | 48 → 0 | 48 → 0 |
| Shader output W, with and without explicit fog (`vs_mode = 2` or `3`) | 72 → 0 | 72 → 0 |
| Full program | 343 → 175 | 345 → 177 |

The remaining failure messages matched the baseline on each GPU.

#### C338 — D3D9 point drawing

Constant zero-size points leave the clear color intact. For a point draw
whose vertex declaration puts PSIZE on an unbound stream, the driver
supplies a size of one pixel.
[Mesa patch](../patches/mesa/d3d9-suppress-zero-size-points-and-default-unbound-psize.patch).

Wine 11.0's `d3d9_visual` ran on helios with Windows 11 template
`26100.1742.11#win3d` and the signed package from `343571b7`. Each GPU
completed 206,068 checks with 31 skips.

| Failed assertions | AMD1 card #253 → C338 | Nvidia2 card #253 → C338 |
| --- | ---: | ---: |
| `visual.c:11460` — size-one draw rejected | 7 → 0 | 7 → 0 |
| `visual.c:11490` — zero-size point changes the clear color | 10 → 0 | 10 → 0 |
| `visual.c:11498` — size-one point leaves the clear color | 7 → 0 | 7 → 0 |
| Full program | 343 → 125 | 345 → 127 |

Card #339 had already cleared the two size-one groups on both GPUs.
On each GPU, the remaining messages matched the failures common to the
table-fog and unbound-stream runs after removing the ten zero-size
failures. The GPU validation for C338 covered `d3d9_visual`.

#### Draft cards

Each row is a draft card. Counts are AMD1 / Nvidia2.

| Draft card | Program and failures | Evidence and completion check |
| --- | --- | --- |
| Accept and render W-buffer depth state | `stateblock`, part of the remaining 18 setter and seven replay failures on AMD1 | `ZENABLE = D3DZB_USEW` returns `E_NOTIMPL` in the driver. Exercise both state-block replay and W-depth rendering. |
| Accept and render D3D9 wireframe fill | `stateblock`, part of the same remaining failures | `FILLMODE = D3DFILL_WIREFRAME` returns `E_NOTIMPL`. Exercise both state-block replay and visible wireframe output. |
| Implement D3D9 cylindrical interpolation | `stateblock`, wrap setter and replay fix above | Draw across a coordinate seam with each wrap component set. Verify the driver interpolates across the seam and honors the flag after applying a state block. |
| Support display rotation changes | `d3d9ex`, 2 / 2 | Lines 573 and 608. Restore the original orientation after a rotation and verify the reported orientation. |
| Restore fullscreen notification and activation behavior | `d3d9ex`, 5 / 9 | Lines 2940–2961, 3007 and 3100. Check `WM_DISPLAYCHANGE` delivery and the foreground window across the fullscreen transition. |
| Match Ex depth-stencil surface creation | `d3d9ex`, 2 / 2 | Lines 687 and 705 are the call and output pointer for the same case. Check both the return value and surface output for test 2. |
| Preserve the uploaded Ex texture contents | `d3d9ex`, 1 / 1 | Line 872 samples `0xf7f7f7` after `UpdateTexture`, expected `0x7f7f7f`. Trace upload and sampling separately. |
| Support the Ex ATI2 volume-texture case | `d3d9ex`, 1 / 1 | Line 4585. Match the expected result for volume-texture test 29. |
| Match pre-transformed vertex fog | `visual`, 24 / 24 | Line 28481, pixel fog disabled, vertex fog enabled, `vs_mode = 0`. Check the fog blend derived from specular alpha. |
| Use shader output specular alpha when oFog is absent | `visual`, 32 / 32 | Line 28481, pixel fog disabled, `vs_mode = 2`. Check the blend for each vertex fog mode with a shader that writes specular alpha. |
| Support fixed-function texture transforms with programmable pixel shaders | `visual`, 64 / 64 | Lines 28754 and 28870. Investigate the 32 draw errors and verify the corresponding texture samples. |
| Preserve dynamic buffer contents across discard and no-overwrite | `visual`, 3 / 3 | Lines 25148 and 26917. Investigate the two map-synchronization failures and the red pixel in the dynamic map draw. |
| Report the expected YUY2 format conversions | `visual`, 2 / 2 | Line 28024 rejects YUY2 to X8R8G8B8 and R5G6B5. Check the format-conversion query for both destinations. |
| Match Nvidia2 ATI1 Fetch4 sampling | `visual`, 0 / 2 | Line 15668 reports two ATI1 readback mismatches in the fixed-function and `texld` cases. Compare the sampled red channel against the test's accepted values. |

## Findings from these runs

Found by earlier runs, fixed on main, and confirmed by the final runs:

- **vulkaninfo crashed** (0xC0000005) in the package's own
  `vulkaninfo.exe`, x64 and x86, as soon as a Venus device enumerated.
  Vulkan-Tools sets up `User32Handles` only under `_MSC_VER`, so the
  llvm-mingw build dereferenced a null `user32_handles` when it created its
  surface window. LunarG's vulkaninfo worked with the same loader and ICD.
  Fixed by `patches/vulkan-tools/vulkaninfo-Load-user32-on-every-Windows-compiler.patch`.
- **dxdiag reported DirectDraw "Not Available"**, so `hardware_acceleration`
  was false even though D3D was `Enabled`. Fixed by
  `patches/mesa/d3d9-directdraw-hal-caps.patch`–`0014` (the D3D9 UMD's DirectDraw HAL).
- **The DirectX SDK's D3D11 (DXUT11) samples did not show.** Fixed by
  `patches/mesa/d3d10umd-hull-shader-vprim-and-indexed-patch-io.patch`–`0017` and explained by DXUT's window placement, as
  described under "Stretch" above. Both now render in the e2e run on both
  templates. The hypothesis recorded here earlier, DXGI output and mode
  enumeration, was wrong (`docs/d3d11-apps.md`).

Open:

- **Winlogon autologon values can vanish across a reboot (cause found,
  guarded).** On fresh windows-11 clones, `AutoAdminLogon` and
  `DefaultUserName` were already gone after `Set-TestDesktop.ps1 -Reboot`,
  before the install ran: in one of the four earlier windows-11 runs and in
  all three windows-11 boots of the AMD final round. vmlab traced it to
  Windows' user OOBE, which deletes those values when it exits and can still
  be running when the clone is reported ready (handoff `20261018-092229`;
  [known limits](known-limits.md#autologon-values-can-vanish-on-a-fresh-clone)).
  `Set-TestDesktop.ps1` now waits for OOBE first. In the five rc1 runs
  the wait took 0 to 25 s (`test-desktop.txt`), the values survived
  (`autologon-*.txt`), and the fallback did not fire.
- **D3D9 ran at about 30 fps** on AMD1 (27 to 34) and about 20 on NVIDIA
  in the 0.1.0 runs, against about 60 for the other APIs. In the rc1 runs
  the D3D9 probe reached 57 to 60 fps on AMD1, 51 to 52 on Nvidia1 (windows-11)
  and 40 to 42 on Nvidia2, after `patches/kmd/kmd-cache-gdi-shadow-and-staging-surfaces.patch` and `patches/mesa/d3d9-ddraw-present-sources-in-host-cached-memory.patch`
  ([`present.md`](present.md)). On windows-10 Nvidia1 the x64 D3D9 probe
  measured 27 fps and the x86 one 46. Below the refresh rate on NVIDIA:
  [known limits](known-limits.md).
- **Blender's window was translucent on Windows 10** in the 0.1.0 runs:
  the wallpaper showed through its viewport and panels, because Windows
  10's compositor honours the alpha Blender leaves in its framebuffer. With
  `patches/mesa/wgl-present-windows-opaque.patch` (WGL presents windows opaque), the rc1 windows-10
  runs on AMD1 and Nvidia1 show Blender opaque
  (`windows-10/apps/console-opengl-x64-Blender.png`,
  `windows-10-nvidia1/apps/console-opengl-x64-Blender.png`).

Fixed after the earlier runs; the final runs exercise part of the fix:

- **`tests/guest/Run-Probe.ps1`** failed on `-Raw` with no `-ArgumentList`,
  because `New-ScheduledTaskAction` rejects an empty `-Argument`. An exit
  code of 0x80000000 or above, such as 0xC0000135 for a missing DLL, threw an
  Int32 conversion error instead of being reported. Commit [cca767b](https://github.com/VMLabDev/vmlab-win3d/commit/cca767b99eb31340488928a85fa25f6b479ff6cb) passes
  `-Argument` only when there is one, and reinterprets the unsigned task
  result as the signed exit code (run.json also gets `exit_code_hex`). Both
  were checked without a guest, in PowerShell 7.4.2 in Docker: all guest
  scripts parse (`Test-GuestScripts.ps1`), and the conversion maps 0xC0000135
  and 0x80000000 to their signed values where the old cast throws. In the
  final runs, the guest's Windows PowerShell 5.1 ran the fixed script for
  every probe and application, with exit codes 0 and 124. A `-Raw` run with
  no arguments, or an exit code of 0x80000000 or above, did not come up.

## Other hosts

These runs used the AMD iGPU on helios. The same Done checks also pass on an
Intel Iris Xe host (`docs/zeus.md`, 12 of 12 with `patches/mesa/yttrium-d3d11-typed-uav-loads-without-formatless-read.patch`) and
on helios's RTX 5090 (below). [Known limits](known-limits.md#host-gpu-support)
has the matrix.

### NVIDIA RTX 5090

Helios has two RTX 5090s, `Nvidia1` (`0000:01:00.0`) and `Nvidia2`
(`0000:05:00.0`). Both need the patched QEMU as their `qemu_binary`, or the
console stays black ([`nvidia.md`](nvidia.md), fault 2). The rc3 runs, with
Resizable BAR on (BAR1 32 GiB), replaced the rc1 runs below
([rc3](#release-candidate-011-alpha-rc3-nvidia-with-resizable-bar)):

```sh
tests/e2e/run.sh --template windows-11 --host-gpu Nvidia1 --lab win3d-rc3-w11-nvidia1 --apps --keep
tests/e2e/run.sh --template windows-11 --host-gpu Nvidia2 --lab win3d-rc3-w11-nvidia2 --apps --keep
tests/e2e/run.sh --template windows-10 --host-gpu Nvidia1 --lab win3d-rc3-w10-nvidia1b --apps
```

The rc1 runs, with BAR1 at 256 MiB:

```sh
tests/e2e/run.sh --template windows-11 --host-gpu Nvidia1 --lab win3d-rc-w11-nv1 --apps --keep
tests/e2e/run.sh --template windows-11 --host-gpu Nvidia2 --lab win3d-rc-w11-nv2 --apps --keep
tests/e2e/run.sh --template windows-10 --host-gpu Nvidia1 --lab win3d-rc-w10-nv1 --apps
```

They passed **12 of 12 each**, with one Venus device, the PCI filter on the
run's own card and every application rendering, with textured meshes. Their
evidence is in git history (above); the tables above are rc3's. Before 0.1.1 the NVIDIA runs differed from AMD1 in three ways, all
fixed in this package:

- The D3D9 `BasicHLSL` sample hung the guest with host Xid 69, so the
  NVIDIA runs had no application results. Fixed by `patches/mesa/yttrium-render-shadow-for-linear-colour-with-depth.patch`
  ([`xid69.md`](xid69.md)).
- After that fix the D3D9 samples' meshes rendered black. Fixed by
  `patches/mesa/yttrium-shared-colour-optimal-when-linear-refused.patch` to `0025` ([`nvtex.md`](nvtex.md)).
- The D3D9 probe ran at about 20 fps ([`present.md`](present.md)).

The 0.1.0-alpha NVIDIA evidence (vmlab 0.27 and 0.28, package built at
[08ff884](https://github.com/VMLabDev/vmlab-win3d/commit/08ff8848dea6defa961a994cbfc7fed8a6d70656) and [99ad0c1](https://github.com/VMLabDev/vmlab-win3d/commit/99ad0c1faee68358bd423fa06548596989f1623c), the stock-QEMU Nvidia2 run with its black
console) is in git history at [82d8e51](https://github.com/VMLabDev/vmlab-win3d/commit/82d8e510c18cf4eaad92bef85ec1ccf14fdd0e75).

## Evidence index

Every directory under [docs/evidence/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/). "Historical" marks evidence from the
vmlab 0.23 era, when labs needed `qemu_args` workarounds (`docs/host.md`). It
is still valid for what it shows, but the package and the lab setup have
changed since.

| Directory | What it proves | vmlab | Package or commit | Status |
| --- | --- | --- | --- | --- |
| [`acceptance/windows-11/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/acceptance/windows-11) | Done items 1 and 2 on a fresh `windows-11` lab on AMD1: install, vulkaninfo, the D3D9, D3D11, D3D12, OpenGL and Vulkan probes on the console, dxdiag, 12 of 12; the stretch apps in `apps/` (Blender's kept capture black, see rc3) | 0.30.0-alpha, `host_gpu = "AMD1"` | 0.1.1-alpha-rc3, the bundle `scripts/release.sh` built at [ee9ce49](https://github.com/VMLabDev/vmlab-win3d/commit/ee9ce49fefc7d6820653b6014d44d876fc12965d); rc2's run is in git history at [ee9ce49](https://github.com/VMLabDev/vmlab-win3d/commit/ee9ce49fefc7d6820653b6014d44d876fc12965d) | Current |
| [`acceptance/windows-10/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/acceptance/windows-10) | The same on a fresh `windows-10` lab on AMD1: 12 of 12; apps, Blender opaque | 0.28.0-alpha, `host_gpu = "AMD1"` | 0.1.1-alpha-rc2 ([0d3eb68](https://github.com/VMLabDev/vmlab-win3d/commit/0d3eb68b7780c21061b34c9768df08aa418497e4)), run at [66809ce](https://github.com/VMLabDev/vmlab-win3d/commit/66809ce4ec3ff77158503fd64cd252c2c5d80e19) | Current |
| [`acceptance/windows-11-nvidia1/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/acceptance/windows-11-nvidia1) | The same on the first RTX 5090 with Resizable BAR, driver 615.78, the 0.1.2-alpha QEMU and render server installed from their release tarballs: 12 of 12, one Venus device; every app renders, no Xid | 0.31.0-alpha, `host_gpu = "Nvidia1"` | 0.1.2-alpha ([45ec46c](https://github.com/VMLabDev/vmlab-win3d/commit/45ec46ca73a5d64b8e0cd956fe7748da2d0c0e17)), the release build; rc3's run is in git history at [45ec46c](https://github.com/VMLabDev/vmlab-win3d/commit/45ec46ca73a5d64b8e0cd956fe7748da2d0c0e17) | Current |
| [`acceptance/windows-10-nvidia1/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/acceptance/windows-10-nvidia1) | The same on a fresh `windows-10` lab on the first RTX 5090 with Resizable BAR: 12 of 12, one Venus device; every app renders, Blender opaque | 0.30.0-alpha, `host_gpu = "Nvidia1"` | 0.1.1-alpha-rc3 ([ee9ce49](https://github.com/VMLabDev/vmlab-win3d/commit/ee9ce49fefc7d6820653b6014d44d876fc12965d)), lab `win3d-rc3-w10-nvidia1b` | Current |
| [`acceptance/windows-11-nvidia2/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/acceptance/windows-11-nvidia2) | The same on the second RTX 5090 with Resizable BAR: 12 of 12, one Venus device, the PCI filter on `0000:05:00.0`, with two other loaded VMs on the card (the ReBAR load check); apps render, Blender's kept capture black (see rc3) | 0.30.0-alpha, `host_gpu = "Nvidia2"` | 0.1.1-alpha-rc3 ([ee9ce49](https://github.com/VMLabDev/vmlab-win3d/commit/ee9ce49fefc7d6820653b6014d44d876fc12965d)); the nvva proof run is in git history at [ee9ce49](https://github.com/VMLabDev/vmlab-win3d/commit/ee9ce49fefc7d6820653b6014d44d876fc12965d) | Current |
| [`rc3/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/rc3/README.md) | Release candidate 0.1.1-alpha-rc3 on NVIDIA with Resizable BAR: the release record (five asset hashes, notes, `BUILDINFO`, the host assets' install check), DirectDraw fullscreen and both D3D9 fixed-function suites on Nvidia1, the ReBAR load check (three VMs on Nvidia2, BAR1 and NVRM monitor), the Blender rerun and the `vmlab cp` hang | 0.30.0-alpha | [ee9ce49](https://github.com/VMLabDev/vmlab-win3d/commit/ee9ce49fefc7d6820653b6014d44d876fc12965d) | Current |
| [`portable/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/portable/README.md) | The host assets built in a Debian 12 container with a minimal QEMU feature set: highest symbol `GLIBC_2.34`, 10 `NEEDED` libraries with virglrenderer 1.3.0 bundled; `install.sh --check`, `--version` and QMP pass on Debian 12 and 13, Ubuntu 24.04, Fedora 44, Arch and Tumbleweed, and Ubuntu 22.04 is refused; windows-11 e2e 12 of 12 on Nvidia1 with both tarballs; Zeus not checked | 0.31.0-alpha, `host_gpu = "Nvidia1"` | 0.1.1-alpha-portable1, built at [53b8044](https://github.com/VMLabDev/vmlab-win3d/commit/53b804494f99ce52ff1f2acb06856b30e4aa862a), unpublished | Current |
| [`compat/rc3-nvidia2/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/compat/rc3-nvidia2) | The compatibility sweep on Nvidia2 with the rc3 package, 12 of 12 ([`compat.md`](compat.md#release-candidate-011-alpha-rc3-on-nvidia2)) | 0.30.0-alpha, `host_gpu = "Nvidia2"` | [ee9ce49](https://github.com/VMLabDev/vmlab-win3d/commit/ee9ce49fefc7d6820653b6014d44d876fc12965d) | Current |
| [`rc2/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/rc2/README.md) | Release candidate 0.1.1-alpha-rc2 on AMD1: the release record (five asset hashes, notes, QEMU and render-server `BUILDINFO`, the host assets' install check), DirectDraw fullscreen rates and both D3D9 fixed-function suites with that package | 0.28.0-alpha | [0d3eb68](https://github.com/VMLabDev/vmlab-win3d/commit/0d3eb68b7780c21061b34c9768df08aa418497e4) | Current |
| [`compat/rc2-amd1/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/compat/rc2-amd1) | The compatibility sweep on AMD1 with the rc2 package ([`compat.md`](compat.md)) | 0.28.0-alpha, `host_gpu = "AMD1"` | [0d3eb68](https://github.com/VMLabDev/vmlab-win3d/commit/0d3eb68b7780c21061b34c9768df08aa418497e4) | Current |
| [`rc/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/rc/README.md) | Release candidate 0.1.1-alpha-rc1: the release record (asset hashes, notes, QEMU `BUILDINFO`), DirectDraw fullscreen rates and the D3D9 fixed-function coverage with that package on AMD1 and Nvidia1, and the host kernel's NVRM lines during the runs | 0.28.0-alpha | [0ee5ba1](https://github.com/VMLabDev/vmlab-win3d/commit/0ee5ba149e10b7671aef8ab3db71f187c53d895c) | Current |
| [`vulkaninfo/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/vulkaninfo/README.md) | The package's patched `vulkaninfo.exe` (`patches/vulkan-tools/vulkaninfo-Load-user32-on-every-Windows-compiler.patch`) lists Venus and exits 0, x64 and x86 | 0.24.0-alpha | [78fc688](https://github.com/VMLabDev/vmlab-win3d/commit/78fc688ec4ae7998d7fab978cd4de3cefed55603) (`work/fix-vulkaninfo`) | Current |
| [`dxdiag/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/dxdiag) | dxdiag's DDraw and AGP status before and after `patches/mesa/d3d9-directdraw-hal-caps.patch` to `0014`, with `ddcaps` DirectDraw checks ([`dxdiag.md`](dxdiag.md)) | 0.24.0-alpha | before: `main` at [be2ef9b](https://github.com/VMLabDev/vmlab-win3d/commit/be2ef9b0ece9d98a33a01ce049dc8fbe846fb6b8); after: [deb2585](https://github.com/VMLabDev/vmlab-win3d/commit/deb258516ab8cdb2c1b48623c01dcb0317ee2ef2) | Current |
| [`d3d11-apps/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/d3d11-apps) | SubD11 and BasicHLSL11: the causes, `patches/mesa/d3d10umd-hull-shader-vprim-and-indexed-patch-io.patch` to `0017`, the DXUT window bug and the `D3D11Ref.dll` workaround ([`d3d11-apps.md`](d3d11-apps.md)) | 0.24.0-alpha | [5fd206d](https://github.com/VMLabDev/vmlab-win3d/commit/5fd206d3c2fd259ad4a2a7e68394771ab9e1cf57) | Current |
| [`nvidia/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/nvidia) | The two RTX 5090 faults and their fixes, `patches/mesa/0020` and [patches/host/qemu/0001](https://github.com/VMLabDev/vmlab-win3d/blob/bea2a93d3ccbfff8b94e321541d573291bc8b4d1/patches/host/qemu/0001-ui-egl-headless-external-only-dmabuf-scanouts.patch); `05-amd-regression/` shows no AMD regression ([`nvidia.md`](nvidia.md)) | 0.24.0-alpha, isolated supervisor | [bea2a93](https://github.com/VMLabDev/vmlab-win3d/commit/bea2a93d3ccbfff8b94e321541d573291bc8b4d1) | Current for the diagnosis; `01-` to `03-` are diagnostic steps. The acceptance runs above replace `04-guest-fix/` as the proof that NVIDIA works |
| [`zeus/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/zeus) | Intel Iris Xe fails on `main` and passes with `patches/mesa/yttrium-d3d11-typed-uav-loads-without-formatless-read.patch`; the AMD regression run with 0021; the RTX 4060 failure ([`zeus.md`](zeus.md)) | 0.24.0-alpha | `main` at [db07da3](https://github.com/VMLabDev/vmlab-win3d/commit/db07da3ced64c042d8a6240b1c54bf6e7d539b91); 0021 at [1bbbfe8](https://github.com/VMLabDev/vmlab-win3d/commit/1bbbfe887760c63c24fd423cd04f8fc18319018e) | Current |
| [`package/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/package/README.md) | A fresh-clone build, then install, probes, rerun, uninstall back to the baseline and reinstall, on a fresh `windows-11` lab | 0.23, with workarounds (`lab-vmlab.wcl`) | [f7fa236](https://github.com/VMLabDev/vmlab-win3d/commit/f7fa236b421a5467531c19da348738774f252698) (`work/package`) | **Historical.** The bundle predates `patches/vulkan-tools/vulkaninfo-Load-user32-on-every-Windows-compiler.patch` and Mesa 0012 on, so the file hashes it quotes are not today's. The uninstall evidence is still the only uninstall run |
| [`kmd/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/kmd/README.md) | The KMD's test signature is accepted and the driver loads, with no Code Integrity events | 0.23 (`display = "virtio-vga-gl"`) | [e166465](https://github.com/VMLabDev/vmlab-win3d/commit/e1664656b92d809c9f771b853e94128b4f89316f) | **Historical**: a kernel-only install with no UMDs, so the device showed problem code 43; the full package's device has problem code 0 ([acceptance/*/installed-gpu-state.json](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/acceptance/windows-10-nvidia1/installed-gpu-state.json)). Signing is unchanged since |
| [`probes/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/probes/README.md) | The probes and guest scripts work on Microsoft's software renderers, before any driver from this repository | 0.23, no GPU | [26ca683](https://github.com/VMLabDev/vmlab-win3d/commit/26ca683dcf14e1f3688d346579ad18b6ec7433dc) | **Historical** |
| [`host/`](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/host) | The host-probe captures `host.md` cites: which QEMU, display and loader settings Venus needs | 0.23, with `qemu_args` variants | [d1bad91](https://github.com/VMLabDev/vmlab-win3d/commit/d1bad91ba6285100b391080e9ab424da8435300e) | **Historical**: vmlab 0.24 emits these settings itself |

## Done item 3

The script does not check item 3. These are the documents that answer it:

| Topic | Document |
| --- | --- |
| Install | `docs/install.md` |
| Uninstall | `docs/uninstall.md` (the run that proves it: [docs/evidence/package/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/package/)) |
| Component stack and upstream pins | `docs/stack.md` (pin and patch tables generated from `build/*/PINS`, [patches/host/PINS](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/patches/host/PINS) and `patches/`; `scripts/pins-table.py --check`) and `build/*/README.md` |
| Signing decision | `docs/decisions/0001-signing.md` |
| Known limits | `docs/known-limits.md`, including the host GPU matrix |
| Evidence for item 2 | This page, the evidence index above, and the investigations it links: `docs/dxdiag.md`, `docs/d3d11-apps.md`, `docs/nvidia.md`, `docs/zeus.md` |

`scripts/check-docs.py` checks that every repository path named in `docs/`
and `README.md` exists.
