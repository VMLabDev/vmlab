# Acceptance pass of `next`, 2026-11-25 (card #886)

This page records one acceptance pass of `next` after the review round: the
full end-to-end suite on vmlab-hv, the scenarios QEMU still runs, and the
win3d Done checks and adapter suite on AMD1. Card #916 added the same win3d
runs on Nvidia2 (R7 to R9). Every failure has a draft fix card in
[Draft fix cards](#draft-fix-cards). The lead files the drafts.

## Summary

| Code | Run | Result |
| --- | --- | --- |
| R1 | e2e on vmlab-hv, every scenario | 180 pass, 2 fail, 10 skip of 192 features. Fails: `vm.macos` (D1), `worktree.isolation.global` (D2) |
| R2 | e2e on QEMU (`--hypervisor qemu`) | `arch.emulated` passes. The other 191 features are skipped: QEMU runs no x86 guest |
| R3 | e2e `vm.hw.gpu` alone, on AMD1 | pass |
| R4 | win3d Done checks, `windows-11` and `windows-10`, AMD1 | 12 of 12 pass on each |
| R5 | win3d adapter suite, `windows-11`, AMD1 | 25 pass, 4 fail, 0 not-run. Fails: `audio` (D3), three Wine programs (D4 to D6) |
| R6 | win3d adapter suite, `windows-10`, AMD1 | 25 pass, 4 fail, 0 not-run. The same four rows |
| R7 | win3d Done checks, `windows-11` and `windows-10`, Nvidia2 (card #916) | 12 of 12 pass on each |
| R8 | win3d adapter suite, `windows-11`, Nvidia2 | 24 pass, 5 fail, 0 not-run. Fails: the four rows of R5, and `wine-d3d9_visual` (D7) |
| R9 | win3d adapter suite, `windows-10`, Nvidia2 | 23 pass, 6 fail, 0 not-run. Fails: the five rows of R8, and `tear-blt` (D8) |

Card #886 did not run the win3d suite on Nvidia2, because Nvidia2 was bound
to `vfio-pci` for passthrough during its pass. Card #916 ran it on
`next` at a later commit, when the overseer had switched Nvidia2 to the host.

## What was tested

- Commit: `edd01cfe2b9b91139f2a7dbbc7cfcd60f9442c6b`, the head of `next` on
  2026-11-24. The branch `card-886` had no other commit during the runs.
- Host: helios, account `vmlab`, 2026-11-25 00:48 to 02:37 local time
  (UTC+10). Other cards ran `just ci::check` on the host at the same time.
  The load average was between 8 and 21 on 32 CPUs.
- GPU: AMD1 is the Radeon 610M at `0000:72:00.0`. It was
  `/dev/dri/renderD129` during this pass, not `renderD130` as the card says,
  because Nvidia2 (`0000:05:00.0`) was on `vfio-pci` and had no render node.
  `renderD128` (Nvidia1, the display GPU) was not used. Every GPU run held
  `/home/vmlab/src/stc/win3d-gpu-AMD1.lock`. The longest hold was 18 minutes.
  The kernel logged no Xid and no amdgpu timeout or reset during any hold.
- All logs and evidence are on helios under `/home/vmlab/src/stc/card-886/`.
  The paths below are relative to that directory.

## End-to-end suite on vmlab-hv (R1)

Command, in the checkout of the commit:

```sh
VMLAB_E2E_LEGACY_STORE=$HOME/.local/share/vmlab/templates \
VMLAB_E2E_MACOS_STORE=$HOME/.local/share/vmlab/templates \
VMLAB_E2E_APPLE_SMC_OSK=<from the host config> \
just e2e::run
```

`VMLAB_E2E_RENDER_NODE` was unset in this run, so `vm.hw.gpu` is a skip here
and has a run of its own (R3). The run took 92 minutes: 32 minutes of image
build, slowed by a 40 KB/s download, and 57 minutes of scenarios. Exit code
1. Logs: `e2e-hv/run.log`, `e2e-hv/e2e.log`, `e2e-hv/results.json`,
`e2e-hv/junit.xml`.

| Scenario | Seconds | Pass | Fail | Skip | Verdict |
| --- | ---: | ---: | ---: | ---: | --- |
| `template` | 197 | 17 | 0 | 0 | pass |
| `variants` | 279 | 3 | 0 | 0 | pass |
| `core` | 696 | 29 | 0 | 0 | pass |
| `firmware` | 37 | 3 | 0 | 0 | pass |
| `sleep` | 55 | 2 | 0 | 0 | pass |
| `guest` | 126 | 22 | 0 | 0 | pass |
| `legacy` | 423 | 5 | 0 | 0 | pass |
| `macos` | 265 | 0 | 1 | 0 | fail |
| `network` | 118 | 18 | 0 | 0 | pass |
| `containers` | 53 | 13 | 0 | 0 | pass |
| `container_workdir` | 3 | 1 | 0 | 0 | pass |
| `snapshots` | 193 | 8 | 0 | 0 | pass |
| `worktree` | 137 | 5 | 1 | 0 | fail |
| `scripting` | 38 | 10 | 0 | 0 | pass |
| `record` | 100 | 12 | 0 | 0 | pass |
| `playbooks` | 0 | 0 | 0 | 4 | skip |
| `dev` | 50 | 7 | 0 | 0 | pass |
| `worktree_clone` | 103 | 2 | 0 | 0 | pass |
| `apps` | 10 | 3 | 0 | 2 | pass |
| `peers` | 45 | 1 | 0 | 0 | pass |
| `admin` | 212 | 12 | 0 | 0 | pass |
| `fastpath` | 133 | 1 | 0 | 0 | pass |
| `unsupported` | 157 | 6 | 0 | 4 | pass |
| Total | 3430 | 180 | 2 | 10 | fail |

### Features that passed

| Scenario | Features |
| --- | --- |
| `template` | `template.build.qcow2`, `template.build.layered`, `template.list`, `template.export`, `template.import`, `template.rm`, `template.clean`, `template.registry.login`, `template.registry.push`, `template.registry.pull`, `template.registry.search`, `template.registry.search-latest`, `template.registry.list-remote`, `template.registry.config`, `template.registry.lab-ref`, `template.event`, `lab.pull` |
| `variants` | `template.variant.build`, `template.variant.select`, `template.variant.rebuild` |
| `core` | `lab.validate`, `lab.validate.reject`, `lab.up`, `lab.up.partial`, `lab.depends_on`, `lab.depends_on_provision`, `lab.provision.parallel`, `lab.status`, `lab.down`, `lab.up.reload`, `lab.reload.stopped`, `lab.reload.network`, `lab.up.template-changed`, `vm.start`, `vm.stop`, `vm.restart`, `vm.destroy`, `vm.ip`, `vm.hw.cpus-memory`, `vm.hw.disk`, `vm.hw.disk-from`, `vm.hw.cdrom`, `vm.hw.floppy`, `vm.hw.nested`, `vm.hw.tsc`, `vm.scratch`, `vm.media.iso`, `vm.media.floppy`, `share.unmountable` |
| `firmware` | `vm.hw.tpm`, `vm.hw.firmware`, `vm.hw.secure_boot` |
| `sleep` | `vm.sleep`, `vm.prevent_sleep` |
| `guest` | `agent.exec`, `agent.exec.timeout`, `agent.shell`, `agent.cp.push`, `agent.cp.pull`, `agent.tail`, `agent.osinfo`, `agent.stats`, `agent.capabilities`, `agent.clipboard`, `agent.rehandshake`, `agent.update`, `agent.repair`, `login.default`, `login.user`, `login.password`, `vision.screenshot`, `vision.ocr`, `vision.find-image`, `vision.sendkeys`, `vision.mouse`, `console.tcp` |
| `legacy` | `vm.legacy.freedos`, `vm.legacy.windows98`, `vm.legacy.windows7`, `vm.legacy.templeos`, `vm.legacy.server2016` |
| `network` | `net.dhcp`, `net.static-ip`, `net.dns.records`, `net.dns.sinkhole`, `net.dns.verb`, `net.nat`, `net.nat.bulk`, `net.isolated`, `net.forward`, `net.forward.bind`, `net.forward.provisioned`, `port.verb`, `net.routes`, `net.l3.block`, `net.l3.redirect`, `net.mtu`, `net.global`, `net.fastpath` |
| `containers` | `share.virtiofs`, `share.smb`, `share.readonly`, `container.up`, `container.idle`, `container.env`, `container.volume`, `container.port`, `container.healthcheck`, `container.exec`, `container.logs`, `container.shell`, `container.power` |
| `container_workdir` | `container.workdir` |
| `snapshots` | `snapshot.vm.online`, `snapshot.vm.offline`, `snapshot.lab`, `snapshot.list-delete`, `snapshot.container`, `snapshot.clock.set`, `snapshot.clock.kept`, `snapshot.clock.fixed` |
| `worktree` | `worktree.isolation.status`, `worktree.isolation.forward`, `worktree.isolation.nat`, `worktree.ports.shift`, `worktree.ports.stable` |
| `scripting` | `script.run`, `script.provision`, `script.as_login`, `script.segment-api`, `script.vision-api`, `script.terminal`, `script.snapshot-api`, `events.handlers`, `logs.streams`, `logs.follow` |
| `record` | `record.status`, `record.up-from-boot`, `record.probe`, `record.timeline`, `record.ocr`, `record.ripple`, `record.compress-idle`, `record.webm`, `record.refusals`, `record.restart`, `record.script`, `record.wscript` |
| `dev` | `dev.sync.host-to-guest`, `dev.sync.guest-to-host`, `dev.sync.modes`, `dev.sync.status-flush`, `dev.sync.conflict`, `dev.select`, `dev.snapshot-bracket` |
| `worktree_clone` | `worktree.clone`, `worktree.clone.macs` |
| `apps` | `app.x11`, `app.interrupt`, `app.exit-code` |
| `peers` | `net.peer` |
| `admin` | `lab.destroy`, `lab.list`, `lab.info`, `lab.stop`, `lab.down.release`, `lab.down.isolation`, `lab.move`, `lab.status.conflict`, `lab.restart`, `lab.destroy.named`, `daemon.lifecycle`, `arch.emulated` |
| `fastpath` | `net.fastpath.kernel` |
| `unsupported` | `vm.hw.audio`, `usb.list`, `usb.controller`, `usb.refusals`, `console.viewer`, `share.smb1` |

### Features that failed

| Feature | Scenario | Failing step | Draft |
| --- | --- | --- | --- |
| `vm.macos` | `macos` | `vmlab up` exits 1: `vmlab-hv exited during startup: exit status: 2`, `reading VGA ROM /usr/share/qemu/vgabios-vmware.bin: No such file or directory` | D1 |
| `worktree.isolation.global` | `worktree` | `nslookup web.e2e-worktree-worktree-isolation-wt.vmlab.internal 10.83.9.1` in the worktree's guest: `connection timed out; no servers could be reached` | D2 |

`worktree.isolation.global` failed again in a run of the scenario alone
(`just e2e::run --only worktree`, exit 1, 5 pass and 1 fail,
`e2e-hv-worktree-rerun/`). It is not a flaky failure. `vm.macos` was not run
again: the missing file is a property of the image.

### Features that were skipped

| Feature | Scenario | Reason |
| --- | --- | --- |
| `vm.hw.gpu` | `unsupported` | `VMLAB_E2E_RENDER_NODE` was unset in the full run. It passes in R3 |
| `usb.attach` | `unsupported` | The e2e container has no host USB device it may take |
| `agent.eventlog` | `unsupported` | Needs a Windows guest |
| `login.elevated` | `unsupported` | Needs a Windows guest |
| `playbook.up`, `playbook.list`, `playbook.check-apply`, `playbook.timeout` | `playbooks` | No config-weave for the account `vmlab` on helios: `~/.local/share/config-weave/bin` does not exist there. It exists for the operator's account |
| `app.waypipe` | `apps` | No Wayland compositor in the e2e container |
| `app.rdp` | `apps` | No Windows guest in the e2e suite |

The four playbook features are the only skips that a change on helios would
remove. They are open: see O1 in [Open points](#open-points).

## Scenarios on QEMU (R2)

QEMU runs no x86 guest since the removal (PRD §22.4). One feature still uses
it: `arch.emulated`, the aarch64 guest of the `admin` scenario.

- `just e2e::run --hypervisor qemu`: exit 0, 1 pass (`arch.emulated`), 191
  skip with the reason `QEMU runs no x86 guest (PRD §22)`. Logs: `e2e-qemu/`.
- The vmlab-hv run (R1) also runs `arch.emulated` on QEMU, through automatic
  selection. It passed there too.

No other architecture has an e2e scenario.

## `vm.hw.gpu` on AMD1 (R3)

`VMLAB_E2E_RENDER_NODE=/dev/dri/renderD129 just e2e::run --only unsupported`,
under the AMD1 lock (held 2 minutes): exit 0, 7 pass, 3 skip (the three
skips of `unsupported` above). `vm.hw.gpu` passed: virgl on vmlab-hv, the
guest's virtio-gpu negotiated `+virgl` and exposes a render node. The
feature does not exercise passthrough or vulkan. Logs: `e2e-hv-gpu/`.

## win3d on AMD1 (R4 to R6)

What ran, all built from the commit above:

- vmlab (`cargo build --release`), the vmlab-hv bundle (`just
  ci::hv-package`, SHA256 `a64bb27c53e7...`), and the Windows and Linux
  agents (`guest/build-agent.sh`).
- The package `vmlab-win3d-0.1.3`, driver `100.6.101.58123`, built by
  `scripts/build.sh` (manifest commit `e136a7a3`, the last commit that
  changed a package source).
- A host config that declares only `gpu "AMD1"`, so no lab could land on
  another GPU.
- Assumption: the SeaBIOS and OVMF firmware came from the operator's install
  on helios (`/home/wil/.local/share/vmlab/guest/firmware`, 2026-11-03), as
  in the runs of card 409. The firmware of the commit is covered by the e2e.

Each template had three holds of the AMD1 lock, with the lab stopped
(`vmlab down`) between them:

1. `tests/e2e/run.sh --template <t> --host-gpu AMD1 --keep`: a fresh clone
   of the plain template, the documented install, the Done checks.
2. `tests/e2e/adapter.sh` with every check except `chromium` and `wine`. The
   lab file got `audio = true` before this hold, so the `audio` row ran.
3. `tests/e2e/adapter.sh <lab> win <dir> chromium wine`, then `vmlab
   destroy`.

| Template | Hold 1 | Hold 2 | Hold 3 |
| --- | --- | --- | --- |
| `x86_64/windows-11@26100.1742.11` | 18 min | 14 min | 19 min |
| `x86_64/windows-10@19045.2006.10` | 18 min | 11 min | 14 min |

Logs: `logs/done-<template>.log`, `logs/adapter-<template>-<1|2>.log`.
Evidence: `ev/<template>/done/`, `ev/<template>/adapter-1/adapter/`,
`ev/<template>/adapter-2/adapter/`.

### Done checks (R4)

Rows marked `+` are extra and do not count towards the 12.

| Item | Check | windows-11 | windows-10 |
| --- | --- | --- | --- |
| 1 | `scripts/build.sh` builds the package | pass | pass |
| 2 | documented install command (`scripts/install-into-vm.sh`) | pass | pass |
| 2+ | display device on our driver, problem code 0 | pass | pass |
| 2 | `vulkaninfo --summary` (x64) lists the Venus device | pass | pass |
| 2+ | `vulkaninfo --summary` (x86) lists the Venus device | pass | pass |
| 2+ | `vulkaninfo --summary` (x64) lists exactly one Venus device | pass | pass |
| 2+ | LunarG vulkaninfo 1.4.363 (x64, pinned) lists Venus through the installed loader | pass | pass |
| 2+ | LunarG vulkaninfo 1.4.363 (x86, pinned) lists Venus through the installed loader | pass | pass |
| 2 | Direct3D 9 (x64) renders on the GPU and shows on the console | pass | pass |
| 2 | Direct3D 9 (x86) renders on the GPU and shows on the console | pass | pass |
| 2 | Direct3D 11 (x64) renders on the GPU and shows on the console | pass | pass |
| 2 | Direct3D 11 (x86) renders on the GPU and shows on the console | pass | pass |
| 2 | Direct3D 12 (x64) renders on the GPU and shows on the console | pass | pass |
| 2 | Direct3D 12 (x86) renders on the GPU and shows on the console | pass | pass |
| 2 | OpenGL (x64) renders on the GPU and shows on the console | pass | pass |
| 2 | OpenGL (x86) renders on the GPU and shows on the console | pass | pass |
| 2+ | Vulkan (x64) renders on the GPU and shows on the console | pass | pass |
| 2+ | Vulkan (x86) renders on the GPU and shows on the console | pass | pass |
| 2 | dxdiag reports hardware acceleration on our driver | pass | pass |
| 2+ | dxdiag: Direct3D acceleration on our full WDDM device | pass | pass |

Both runs end with `PASS: 12 of 12 Done checks pass` and exit 0.

### Adapter suite (R5, R6)

`adapter.sh` exited 1 in every hold, because a check failed in each.

| Property | Row | windows-11 | windows-10 | Draft |
| --- | --- | --- | --- | --- |
| P1 | `pointer` | pass | pass | |
| P1 | `pointer-shapes` | pass | pass | |
| P2 | `vblank-interval` | pass, mean 16.667 ms | pass, mean 16.665 ms | |
| P2 | `present-rate` | pass, 60.21 frames per second | pass, 60.21 frames per second | |
| P2 | `flip-timestamps` | pass, 59.99 Hz | pass, 60.00 Hz | |
| P2 | `scan-line` | pass, 768 lines | pass, 768 lines | |
| P3-P5 | `tear-flip` | pass, 0 torn of 1260 frames | pass | |
| P3-P5 | `tear-blt` | pass, 0 torn of 1259 frames | pass | |
| P7 | `duplication` | pass | pass | |
| P7 | `shared-texture` | pass | pass | |
| P10 | `d2d` | pass | pass | |
| P10 | `d2d-tir` | pass | pass | |
| P10 | `draws` | pass | pass | |
| P10 | `svb-chrome` | pass | pass | |
| P8 | `venc-chrome` | pass, 5 of 5 tries, 0 crash dumps | pass, 5 of 5 tries, 0 crash dumps | |
| P10 | `svb-edge` | pass | pass | |
| P8 | `venc-edge` | pass, 5 of 5 tries, 0 crash dumps | pass, 5 of 5 tries, 0 crash dumps | |
| P8 | `chromium` | pass | pass | |
| P10 | `wine-d3d10_1_d3d10_1` | pass, 580 tests | pass, 580 tests | |
| P10 | `wine-d3d10core_d3d10core` | pass, 195912 tests | pass, 195912 tests | |
| P10 | `wine-d3d10_device` | pass, 206 tests | pass, 206 tests | |
| P10 | `wine-d3d10_effect` | pass, 5041 tests | pass, 5041 tests | |
| P10 | `wine-d3d11_d3d11` | **fail**, 2 failures of 5561777 | **fail**, 2 failures of 5561777 | D4 |
| P10 | `wine-d3d9_d3d9ex` | **fail**, 8 failures of 2387 | **fail**, 15 failures of 2398 | D5, D6 |
| P10 | `wine-d3d9_device` | **fail**, 10 failures of 56034 | **fail**, 34 failures of 56071 | D6 |
| P10 | `wine-d3d9_stateblock` | pass, 14738 tests | pass, 14738 tests | |
| P10 | `wine-d3d9_visual` | pass, 207441 tests | pass, 207441 tests | |
| U1 | `latency` | pass, p50 21 ms, p90 31 ms | pass, p50 31 ms, p90 41 ms | |
| S1 | `audio` | **fail**, 22 discontinuities, 1 silent gap of 5 ms | **fail**, 2 discontinuities, 0 gaps | D3 |

Against the baseline in `guest/win3d/docs/acceptance.md` (2026-11-20, AMD1):

- No row changed from pass to fail.
- `wine-d3d10core_d3d10core`, `wine-d3d9_stateblock` and `wine-d3d9_visual`
  failed there and pass now. `scan-line`, `duplication`, `venc-chrome` and
  `venc-edge`, which failed in the earlier baseline, pass.
- `wine-d3d11_d3d11` and `wine-d3d9_device` ended on an exception or on a
  stopped VM there. Both now run to their summary line.
- Card #246 (vmlab-hv exits with "Too many open files" during Wine's d3d9
  `device` test) did not repeat: the program ran to its end on both
  templates, in 96 s and 75 s.
- `latency` and `audio` are not in that baseline. Their first numbers are in
  `guest/win3d/docs/acceptance/C447.md`.

## win3d on Nvidia2 (R7 to R9, card #916)

The same Done checks and adapter suite as R4 to R6, on the second RTX 5090.

What ran, all built on helios from `ab017e1b08ccd2db0e9529b09174aacafbd0d7e7`,
the head of `next` on 2026-11-24 (51 commits after the commit of R4 to R6,
among them the vmlab-hv sound and virtio fixes `22977ea1` and `1e59f81f`):

- vmlab (`cargo build --release`), the vmlab-hv bundle (`just
  ci::hv-package`, SHA256 `38f5df0c693a...`), run through its launcher
  `hv/bin/vmlab-hv`, and the Windows and Linux agents
  (`guest/build-agent.sh`).
- The package `vmlab-win3d-0.1.3`, built by `scripts/build.sh`.
- A host config that declares only `gpu "Nvidia2" { address =
  "0000:05:00.0" }`, so no lab could land on another GPU. `vmlab status -v`
  showed `host_gpu=Nvidia2`, `VK_LOADER_DRIVERS_SELECT=*nvidia*` and
  `VMLAB_VK_PCI_ADDRESS=0000:05:00.0` for every lab.
- The firmware came from the operator's install, as in R4 to R6.

GPU and host:

- Nvidia2 is the RTX 5090 at `0000:05:00.0`, driver `nvidia` 615.78.08. It
  was `/dev/dri/renderD130` during this pass, not `renderD129` as card #916
  says: `renderD129` is AMD1 (`0000:72:00.0`) when Nvidia2 is on the host.
  `renderD128` (Nvidia1) was not used, and no device was bound or unbound.
- Every GPU run held `/home/vmlab/src/stc/win3d-gpu-Nvidia2.lock`. The
  longest hold was 25 minutes. At the end every lab of the pass was
  destroyed, the pass's supervisor was stopped, and `nvidia-smi` listed no
  process on `0000:05:00.0`.
- Xid: the kernel log is closed to the accounts `vmlab` and `meta` on
  helios (`journalctl -k` returns no line, `dmesg` fails with `Operation not
  permitted`). An Xid was therefore watched by its symptom only: no VM log
  of a lab had `Xid`, `DEVICE_LOST` or `device lost`, every Done check and
  adapter row ran to its verdict, and `nvidia-smi` answered after every hold.
  The same limit applies to the statement in "What was tested" that the
  kernel logged no Xid during the AMD1 holds.
- Host: helios, account `vmlab`, 2026-11-25 05:25 to 07:39 local time
  (UTC+10).
- Logs and evidence are on helios under `/home/vmlab/src/stc/card-916/`:
  `logs/done-<template>.log`, `logs/adapter-<template>-<1|2>.log`,
  `logs/gpu-*.txt` (`nvidia-smi` after each run), `ev/<template>/done/`,
  `ev/<template>/adapter-<1|2>/adapter/`, and the repeat of D8 in
  `logs/rerun-windows-10.log` and `ev/windows-10/rerun-<1|2|3>/adapter/`.

The holds were the three of R4 to R6, with `--host-gpu Nvidia2`:

| Template | Hold 1 (Done checks) | Hold 2 (adapter, all but `chromium` and `wine`) | Hold 3 (`chromium`, `wine`) |
| --- | --- | --- | --- |
| `x86_64/windows-11@26100.1742.11` | 16 min | 15 min | 25 min |
| `x86_64/windows-10@19045.2006.10` | 14 min | 11 min | 24 min |

A fourth hold of 21 minutes repeated `tear-blt` on `windows-10` (D8). The
scripts of the pass are in `card-916/bin/`.

### Done checks (R7)

All 20 rows of the R4 table pass on both templates, the extra rows
included. Both runs end with `PASS: 12 of 12 Done checks pass` and exit 0.

### Adapter suite (R8, R9)

`adapter.sh` exited 1 in every hold, because a check failed in each. The
AMD1 columns are R5 and R6.

| Property | Row | windows-11, Nvidia2 | windows-10, Nvidia2 | AMD1 | Draft |
| --- | --- | --- | --- | --- | --- |
| P1 | `pointer` | pass | pass | pass | |
| P1 | `pointer-shapes` | pass | pass | pass | |
| P2 | `vblank-interval` | pass, mean 16.665 ms | pass, mean 16.661 ms | pass | |
| P2 | `present-rate` | pass, 60.11 frames per second | pass, 59.83 frames per second | pass | |
| P2 | `flip-timestamps` | pass, 59.99 Hz | pass, 59.80 Hz | pass | |
| P2 | `scan-line` | pass, 768 lines | pass, 768 lines | pass | |
| P3-P5 | `tear-flip` | pass, 0 torn of 1260 frames | pass, 0 torn of 1260 frames | pass | |
| P3-P5 | `tear-blt` | pass, 0 torn of 1264 frames | **fail**, 1 torn of 1261 frames | pass | D8 |
| P7 | `duplication` | pass | pass | pass | |
| P7 | `shared-texture` | pass | pass | pass | |
| P10 | `d2d` | pass | pass | pass | |
| P10 | `d2d-tir` | pass | pass | pass | |
| P10 | `draws` | pass | pass | pass | |
| P10 | `svb-chrome` | pass | pass | pass | |
| P8 | `venc-chrome` | pass, 5 of 5 tries, 0 crash dumps | pass, 5 of 5 tries, 0 crash dumps | pass | |
| P10 | `svb-edge` | pass | pass | pass | |
| P8 | `venc-edge` | pass, 5 of 5 tries, 0 crash dumps | pass, 5 of 5 tries, 0 crash dumps | pass | |
| P8 | `chromium` | pass | pass | pass | |
| P10 | `wine-d3d10_1_d3d10_1` | pass, 580 tests | pass, 580 tests | pass | |
| P10 | `wine-d3d10core_d3d10core` | pass, 195912 tests | pass, 195912 tests | pass | |
| P10 | `wine-d3d10_device` | pass, 206 tests | pass, 206 tests | pass | |
| P10 | `wine-d3d10_effect` | pass, 5041 tests | pass, 5041 tests | pass | |
| P10 | `wine-d3d11_d3d11` | **fail**, 2 failures of 5561777 | **fail**, 2 failures of 5561777 | fail, the same 2 lines | D4 |
| P10 | `wine-d3d9_d3d9ex` | **fail**, 8 failures of 2387 | **fail**, 17 failures of 2399 | fail | D5, D6 |
| P10 | `wine-d3d9_device` | **fail**, 14 failures of 56034 | **fail**, 32 failures of 56065 | fail | D6 |
| P10 | `wine-d3d9_stateblock` | pass, 14738 tests | pass, 14738 tests | pass | |
| P10 | `wine-d3d9_visual` | **fail**, 2 failures of 207441 | **fail**, 2 failures of 207441 | pass | D7 |
| U1 | `latency` | pass, p50 24 ms, p90 34 ms | pass, p50 35 ms, p90 44 ms | pass | |
| S1 | `audio` | **fail**, 3 discontinuities, 0 gaps | **fail**, 1 discontinuity, 0 gaps | fail | D3 |

Failures that AMD1 does not have, compared by file and line against the
failure lists of R5 and R6:

- `wine-d3d9_visual`, both templates: the two ATI1 readbacks at
  `visual.c:15668`. Draft D7.
- `tear-blt`, `windows-10` only: one torn frame. Draft D8.
- `wine-d3d9_device`, both templates: the focus and foreground checks at
  `device.c:4656`, `4658`, `4675` and `4677`. These are the
  session-dependent failures of run 6 of
  [C414](../../guest/win3d/docs/acceptance/C414.md), which has the same four
  lines on AMD1 without Win3D's fix. They belong to D6 and have no draft of
  their own.
- `wine-d3d9_d3d9ex`, `windows-10`: `d3d9ex.c:3132` (message 0x46 not
  received), two more unexpected messages at `d3d9ex.c:2520` (0x7c and
  0x7d), and the mode check at `3022` and `3024` twice. R6 instead failed
  `2986`, `2990` and `3009`. These are the message and mode failures of
  the `windows-10` session that D6 describes, and they also have no draft
  of their own.

The other failed lines are those of R5 and R6. `wine-d3d9_device` on
`windows-10` failed `device.c:3790` 24 times here and 30 times in R6.

Audio (D3): both templates have fewer breaks than R5 and R6, and vmlab-hv
logged 0 underruns, 0 frames of silence and 0 late callbacks in both. The
judge counts, on `windows-11`, 3 discontinuities with 1 stretch of a guest
period, 0 shifts of a host quantum and 1 other, and on `windows-10` 1
discontinuity of the other kind. The tone ran with 16 busy
host processes, as in R5 and R6. The row still fails its limit of no
break. The commit includes `22977ea1` (the sound device holds what a full
sink cannot take), which R5 and R6 did not have.

## Draft fix cards

Each draft has the title, the failing step, the log, and a first reading of
the cause. A first reading is not a diagnosis.

### D1. vmlab-hv needs a VGA ROM for the VMware SVGA display that vmlab does not give it

- Failure: `vm.macos` (R1).
- Failing step: `vmlab up` of `e2e-macos`. vmlab-hv exits 2 at startup:
  `building machine vmlab:e2e-macos/mac: reading VGA ROM
  /usr/share/qemu/vgabios-vmware.bin: No such file or directory`.
- Log: `e2e-hv/e2e.log`, line 7643 and the lines before it.
  `e2e-hv/results.json`, row `vm.macos`.
- First reading: `src/labd/hypervisor/vmlab_hv/describe.rs:662` passes no
  ROM for `DisplayDevice::VmwareSvga`, with the comment that OVMF drives the
  device through its GOP. `hv/crates/vmlab-hv-vmm/src/builder.rs:997` then
  reads a ROM anyway, from a fixed path. That path exists on Arch (helios)
  and not on Debian: the e2e image has the file as
  `/usr/share/seabios/vgabios-vmware.bin` and has no `vgabios-*` file in
  `/usr/share/qemu`. A macOS guest therefore starts on helios and fails in
  the e2e container, and it would fail on a Debian or Ubuntu host too. The
  std, virtio and cirrus ROMs do not have this fault, because vmlab finds
  them and passes each path.
- Fix to consider: vmlab-hv builds the VMware SVGA device without a ROM
  when the description gives none, or vmlab finds `vgabios-vmware.bin` the
  way it finds the other three. Done when `just e2e::run --only macos`
  passes with the OSK set.
- Known card: none found. #151 and #601 are about the template.

### D2. A worktree's global segment gets no DHCP and no DNS when it declares the main lab's subnet

- Failure: `worktree.isolation.global` (R1), twice.
- Failing step: in the worktree lab's guest, `dhcpcd -1 -4 eth1` returns
  after 9.3 s, then both `nslookup <name> 10.83.9.1` calls time out. The
  check expects the worktree's own name to resolve and the main lab's name
  not to. `vmlab status` of both labs shows the segment as `10.83.9.0/24`
  with gateway `10.83.9.1`, and the worktree's as joined
  `shared@worktree-isolation-wt`.
- Log: `e2e-hv/e2e.log`, the lines before `FAIL
  worktree.isolation.global`. The rerun: `e2e-hv-worktree-rerun/e2e.log`,
  from line 19808.
- First reading: commit `293a28c3` (2026-11-24, `fix(network): reuse global
  subnets after last detach`) made `src/supervisor/global.rs:683` refuse a
  declared subnet that overlaps any live global segment. A worktree lab
  reads the same lab file as its main lab, so `shared@<worktree>` always
  declares the subnet that `shared` already holds. The attach is then
  refused and the worktree's segment has no services, while `up` and
  `status` do not say so. The feature passed in the group test of
  2026-11-22, before that commit. Not checked: the supervisor's log of the
  run, which the container took with it.
- Fix to consider: the overlap rule must let the `<segment>@<worktree>`
  copies of one declaration share a subnet (PRD §21.4 gives each its own
  switch and DNS), and a refused attach must fail `up` or warn. Done when
  `just e2e::run --only worktree` passes.
- Known card: none found.

### D3. win3d: the `audio` row fails with CPUs to spare (follow-up of C447)

- Failure: row S1 `audio`, both templates (R5, R6).
- Failing step: `tests/e2e/adapter.sh <lab> win <dir> audio`, a 60 s 997 Hz
  tone captured from the node `vmlab.<lab>.win`, with 16 busy host
  processes. The limit is no discontinuity and no gap.
  - windows-11: 22 discontinuities and 1 silent gap of 5 ms in 56.7 s. 4
    shifts of whole guest periods, 0 shifts of whole host quanta, 16 other.
    vmlab-hv logged 2 underruns, 286 frames of silence, 1 late callback
    (longest 11.2 ms).
  - windows-10: 2 discontinuities, 0 gaps in 59.2 s. 1 shift of a guest
    period, 1 shift of a host quantum. vmlab-hv logged 0 underruns.
- Log: `ev/<template>/adapter-1/adapter/tone.capture.json`,
  `tone.host.json`, and `ev/<template>/adapter-1/vmlab-hv.log`.
- First reading: the causes K1 (the guest refills a period late and the HDA
  model plays the old one), K2 (a late PipeWire callback) and K4 (frames
  lost in the account's PipeWire or in `pw-record`, which have no real-time
  priority) of `guest/win3d/docs/acceptance/C447.md`. That page measured 0
  to 9 breaks at the same load of 16 and left the work A1 for a follow-up
  card. The host also ran three `just ci::check` jobs and the e2e suite
  during the windows-11 tone, which is more load than the check adds. The
  windows-11 underruns are new against C447, which saw none in any run.
- Fix to consider: A1 of C447 (the HDA model must not fetch a period the
  guest has not refilled). Before that, repeat the row on a quiet host to
  separate the host's part.
- Known card: no open card found for A1 of C447.

### D4. win3d: Direct3D 11 pixel shaders with more than 32 combined texture slots draw nothing

- Failure: row `wine-d3d11_d3d11`, both templates, 2 failures each.
- Failing step: Wine's `test_high_resource_count`: `d3d11.c:36343` and
  `d3d11.c:36355` read (0, 0, 0, 0) where (1274, 637, 1225, 0) and (1274,
  637, 1325, 0) are expected.
- Log: `ev/<template>/adapter-2/adapter/wine-d3d11_d3d11.failures.txt`.
- First reading: the limit documented in
  `guest/win3d/docs/acceptance/C375.md`. The UMD's sampler binding map has
  32 combined slots, the shader needs 150, and `CreatePixelShader`
  substitutes a zero-output shader. The 11 NV12 failures and the
  `B8G8R8X8_UNORM` failure of the earlier runs are gone.
- Known card: none open. C375 changed documentation only.

### D5. win3d: `CreateDepthStencilSurfaceEx` with a restricted usage fails

- Failure: row `wine-d3d9_d3d9ex`, both templates, 2 of its failures.
- Failing step: `d3d9ex.c:687` (`Test 2: Got unexpected hr 0x8876086c`,
  `D3DERR_INVALIDCALL`) and `d3d9ex.c:705` (the surface pointer is null).
- Log: `ev/<template>/adapter-2/adapter/wine-d3d9_d3d9ex.failures.txt`.
- First reading: the draft "Match Ex depth-stencil surface creation" in
  `guest/win3d/docs/acceptance.md`, unchanged since C343 (2 failures before
  and after).
- Known card: none open.

### D6. win3d: fullscreen notifications and activation in the Wine d3d9 tests

- Failure: rows `wine-d3d9_d3d9ex` (the other 6 failures on windows-11, 13
  on windows-10) and `wine-d3d9_device` (10 on windows-11, 34 on
  windows-10).
- Failing step, windows-11: `WM_DISPLAYCHANGE` (message 0x7e) is expected
  and not received at `d3d9ex.c:2949`, `3007`, `3100` and at
  `device.c:4248`, `4319`, `4410`. The foreground window is wrong at
  `d3d9ex.c:2915`. The device window styles differ at `device.c:5179` to
  `5220`. `device.c:15008` gets the window rectangle (0,0)-(1023,767) where
  (0,0)-(1024,768) is expected.
- Failing step, windows-10: unexpected messages 0x46, 0x47, 0x86, 0x6,
  0x281 and 0x7 at `d3d9ex.c:2520` and `device.c:3790`, a mode change that
  returns 0x8760878 and leaves 1024x768 at `d3d9ex.c:2986` to `3024`,
  `device.c:4545` to `4568`, and `device.c:15291` (`Got refcount 27`).
- Log: `ev/<template>/adapter-2/adapter/wine-d3d9_d3d9ex.failures.txt` and
  `wine-d3d9_device.failures.txt`.
- First reading: three groups that the win3d notes already separate.
  - The missing `WM_DISPLAYCHANGE` is the draft "Restore fullscreen
    notification and activation behavior" (C340, draft A2 of C413). It is
    the part a driver change can fix.
  - The window styles at `device.c:5179` to `5220` come from the Windows
    runtime and repeat without Win3D (C413). `device.c:15008` is also in
    the Win3D run of C413, with no cause recorded there.
  - The message and focus failures of the windows-10 run, and
    `device.c:15291`, match the session-dependent failures of C412 and
    C414: the control runs without Win3D fail the same lines when another
    window holds the foreground.
- Fix to consider: fix the `WM_DISPLAYCHANGE` delivery, and make the judge
  of the two rows compare against the control list of C413, so that a
  failure of the session or of the Windows runtime does not fail the row.
- Known card: none open. #253 (done) grouped these failures.

### D7. win3d: Wine's d3d9 `visual` ATI1 readbacks fail on Nvidia2 (NVIDIA BC4 decoding)

- Failure: row `wine-d3d9_visual`, both templates on Nvidia2 (R8, R9), 2
  failures each. AMD1 passes the row (R5, R6).
- Failing step: `visual.c:15668`, `Test FFP on ATI1` and `Test texld on
  ATI1`: both read `0x70707070` at (360, 30). Wine expects red `0x6d` with
  a tolerance of 2.
- Log: `card-916/ev/<template>/adapter-2/adapter/wine-d3d9_visual.failures.txt`.
- First reading: the same two lines and the same value as
  [C359](../../guest/win3d/docs/acceptance/C359.md) on Nvidia2 in
  2026-11. ATI1 becomes `VK_FORMAT_BC4_UNORM_BLOCK` on the host, and
  NVIDIA's BC4 decoder gives 112 for this texel where the reference
  decoding gives 109. The Direct3D 11.3 BC error rule allows the
  difference. The driver does not change the value, so a driver fix is
  not likely.
- Fix to consider: one of the two that C359 leaves open. Accept the
  readback in the judge of the row on an NVIDIA host (for example a list
  of known host-precision lines for each host GPU vendor), or decode ATI1
  in a shader and not through the host's BC4 decoder. Done when the row
  passes on Nvidia2 and still passes on AMD1.
- Known card: none open on the board. The draft "Match Nvidia2 ATI1 Fetch4
  sampling" in `guest/win3d/docs/acceptance.md` is the same failure.

### D8. win3d: one torn frame in `tear-blt` on Nvidia2, windows-10

- Failure: row `tear-blt`, `windows-10` on Nvidia2 (R9). `windows-11` on
  Nvidia2 and both templates on AMD1 pass.
- Failing step: `tests/e2e/adapter.sh <lab> win <dir>` in hold 2: 1 torn
  of 1261 console frames, 35.6 grabs per second. The probe presented 1229
  frames at 58.6 per second.
- Log: `card-916/ev/windows-10/adapter-1/adapter/tear-blt.json`,
  `tear-blt.grab.json` and the frame `tear-blt-torn.png`, and the same files
  in `card-916/ev/windows-10/rerun-3/adapter/`.
- Repeat: a fourth hold (`card-916/bin/rerun.sh`) made a fresh
  `windows-10` lab (its Done checks passed 12 of 12 again) and ran the row
  alone three times. Runs 1 and 2 passed with 0 torn of 1260 frames. Run 3
  failed with 1 torn of 1260 frames. So the row fails in 2 of 4 runs on
  `windows-10` on Nvidia2, each time with one frame.
- First reading: the KMD carries a Blt present out as a CPU copy into the
  scanout allocation ([present.md](../../guest/win3d/docs/present.md)), not
  as a flip, so a console grab during the copy can see parts of two
  frames. present.md records 0 torn frames for this row on both GPUs. One
  torn frame in about 1260 is too few to find a cause from these runs alone.
- Fix to consider: run the row 10 times on Nvidia2 with `windows-11` and
  `windows-10`, and on AMD1, to get a rate. If it repeats only on Nvidia2, look at what in the copy path
  depends on the host GPU.
- Known card: none open on the board.

## Open points

| Code | Point |
| --- | --- |
| O1 | The four `playbook.*` features did not run. They need config-weave in `~/.local/share/config-weave/bin` of the account that runs the suite on helios. |
| O2 | Done by card #916: the win3d suite ran on Nvidia2 in vulkan mode (R7 to R9). |
| O3 | The `audio` row should be repeated on a host with no other heavy job before D3 is sized. |
| O4 | The win3d baseline tables in `guest/win3d/docs/acceptance.md` are from 2026-11-20 and no longer match: seven rows that fail there pass now. This card did not change them. |
