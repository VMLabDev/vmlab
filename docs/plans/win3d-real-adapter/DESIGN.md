# win3d: make the Venus device behave like a real video card

Design for card #141. It covers the eight open win3d problems (cards #133 to
#140), what Windows expects from a video card, the ways to get there, a
recommendation, and the work split into cards. It contains no driver code and
no device code.

Codes used below: `F` finding, `P` property of a real video card, `H`
hypothesis that is not verified, `O` option, `R` risk, `C` card, `Q` decision
for the operator.

## Summary

- F1: Four of the eight problems are gaps of the adapter model (class A):
  #134, #137, #139, #140. The other four are not. #135 and #138 are faults of
  the user-mode driver. #136 and item 1 of #133 are host faults. A new device
  model that keeps Venus and the Yttrium user-mode driver would end at most
  four of the eight. The assumption "a real video card model ends all of
  these" is therefore only half right.
- F2: Seven of the eight have a point fix that does not wait for this design.
  Six of them already have a patch in the issue text. Only #134 needs
  investigation first.
- F3: The recommendation is O2: keep the virtio-gpu Venus device and the
  Yttrium driver, complete the adapter in the guest, and extend the vmlab-hv
  device only where the host alone knows the answer (when a frame is finished,
  when a vblank happens, that the host GPU is lost). Add an adapter test suite
  to the win3d acceptance run, so that faults like #135 and #138 are found by
  us and not by users.
- F4: The operator must decide four things: the way (Q1), the relation to the
  upstream driver (Q2), driver signing (Q3) and a native Direct3D 12 driver
  (Q4).

## How this was done, and what was not looked at

Read: `guest/win3d/` (docs, `build/*/PINS`, `patches/`), the vmlab-hv
virtio-gpu device (`hv/crates/vmlab-hv-devices/src/virtio/gpu/`), `hv/docs/`,
`src/qemu/hostgpu.rs`, and the text of cards #133 to #140.

Read from upstream on 2026-11-19, without a build: `arehnman/yttrium-virtio-gpu`
at the pinned `b99d951a` and at the tag `yttrium-experimental-2026-11-02`
(`e9917ae2`), and the commit list and file list of `arehnman/virtio-win-mesa`
at `374c3022` (pinned) and `76c095bb` (November).

Not done:

- No guest was booted. The store of the account vmlab on helios holds
  `x86_64/windows-11@26100.1742.10#win3d` (variant built 2026-11-02) and
  `x86_64/windows-10@19045.2006.9#win3d`. The newest versions
  (`windows-11@26100.1742.11`, `windows-10@19045.2006.10`) have only the
  `standard` variant. Which package version the stored variants hold is not in
  their metadata and was not read from the disk.
- No measurement was repeated. Every number below comes from the issue it is
  cited from.
- All measurements in #134 were taken on QEMU with `egl-headless`. Current
  labs run on vmlab-hv, whose scanout path is different. Nobody has repeated
  the #134 measurement on vmlab-hv. Card C7 does that first.
- The source of `yttrium12.dll` and `d3d11on12.dll`, which the November INF
  installs, was not found in the file lists of either upstream repository.

## 1. How the Windows 3D path is built today

`guest/win3d/docs/stack.md` is the full description. The points that matter
for this design:

| Layer | What it is | Pin, patches |
| --- | --- | --- |
| Device | virtio-gpu (`PCI\VEN_1AF4&DEV_1050`) with the Venus capset and blob resources. On vmlab-hv it is the in-tree device (`hv/crates/vmlab-hv-devices/src/virtio/gpu/`) over rutabaga 0.1.80. On QEMU it is `virtio-vga-gl,venus=true,blob=true`. | In tree |
| Kernel driver | Yttrium `viogpu3d.sys`, a WDDM 2.0 display miniport, a fork of the virtio-win `viogpu` driver by one upstream author. | `arehnman/yttrium-virtio-gpu` `b99d951a` (tag `yttrium-experimental-2026-09-27`), 4 patches in `guest/win3d/patches/kmd` |
| Direct3D 9, 10, 11 | `viogpu_d3d10.dll`: Mesa's `d3d10umd` front end on the Yttrium Gallium driver, with its own Venus transport. Registered in the class key as a normal user-mode driver. | `arehnman/virtio-win-mesa` `374c3022` (September release, Mesa 26.1.0), 47 patches in `guest/win3d/patches/mesa` |
| Vulkan | Mesa's Venus ICD `libvulkan_virtio.dll`, which reaches the kernel driver with `D3DKMTEscape`. | Same Mesa tree |
| OpenGL | Mesa WGL with Zink on the Venus ICD. | Same Mesa tree |
| Direct3D 12 | No driver. The installer replaces `System32\d3d12.dll` with a bridge that loads vkd3d-proton 3.0.1 and DXVK's DXGI. | 6 vkd3d-proton patches, 1 DXVK patch |
| Host renderer | virglrenderer 1.3.0 and its `virgl_render_server`, with 3 patches in `patches/virglrenderer`, on the host's Vulkan driver (RADV, ANV or NVIDIA). vmlab adds a Vulkan layer that leaves the renderer one GPU (`src/qemu/vk_pci_filter.c`). | vmlab-hv bundle |
| Scanout and console | vmlab-hv exports the scanout as a dma-buf, or copies it through an EGL pool, and publishes it to the native frame protocol and to RFB on `vnc.sock`. The cursor queue is implemented and reaches RFB (rich and alpha cursor), the viewer and the recorder. | In tree |
| Templates | The `#win3d` variant of the Windows 10 and 11 templates: the standard variant plus the driver package, test-signed, `secure_boot = false`. Built in the vmlab-templates repository. | Card #133 item 5 says its `fetch-deps.sh` pinned package 0.1.1-alpha |

How the display part of the kernel driver works today (read at `b99d951a`.
`viogpu_vidpn.cpp` is byte-identical at `e9917ae2`):

- F5: One VidPN source and one target. Modes come from the device's display
  info. `vgpusrv` and `viogpuap` follow host resizes.
- F6: Vsync is a guest timer of 16.67 ms (`kVsyncPeriod100ns`). On each tick
  the driver sends an empty command to the host so that the answer raises the
  interrupt that it reports as `DXGK_INTERRUPT_CRTC_VSYNC`. `GetScanLine`
  always answers line 0, not in vertical blank. The timer has no relation to
  when the host shows a frame.
- F7: A flip is recorded by `SetVidPnSourceAddress` and applied on the next
  timer tick: `SET_SCANOUT_BLOB` and `RESOURCE_FLUSH` of the full screen on
  the control queue. Neither command waits for the rendering of the frame.
  The driver reports `FlipOnVSyncMmIo`, `MaxQueuedFlipOnVSync = 1`,
  `SupportDirectFlip` and no overlay planes.
- F8: The pointer capability block is commented out ("Disable pointer on
  viogpu3d for now") and both pointer entry points return
  `STATUS_NOT_IMPLEMENTED`. The same at `e9917ae2`.
- F9: A present of the Blt kind (windowed Direct3D 9, DirectDraw, blt-model
  DXGI swap chains, Desktop Duplication) is a copy on the guest CPU from a
  mapped Venus blob into a GDI surface. A blob destination is refused.
- F10: Rendering does not pass through the Windows GPU scheduler in the way
  it does on a real card. Each guest process has its own Venus context, which
  is its own Vulkan device and queue in a host render worker. The host GPU
  runs those queues side by side.

What the November upstream release changes (commit subjects of
`b99d951a..e9917ae2`, and the diff size of the Mesa fork: 80 files, +7848,
-4831, as one squashed commit):

- F11: Kernel driver: DirectFlip validation for blob primaries, per-process
  views of blob mappings, a clock calibration, unwind fixes, and an INF that
  registers a fourth user-mode driver, `yttrium12.dll`, with `d3d11on12.dll`
  in the driver store. Pointer, vsync timer, flip path and CPU present copy
  are unchanged.
- F12: Mesa fork: the fix for #135 (card #135 names the lines), and changes in
  `d3d10umd`, the Venus ICD and the Win32 WSI that our 47 patches must be
  rebased over.

## 2. The eight problems

Classes: A, the guest does not see what a real video card gives. B, a fault
in the host path. C, a fault of the upstream driver that a newer release
fixes.

| Card | Root cause | Class | Fixed upstream in November | Point fix now |
| --- | --- | --- | --- | --- |
| #133 item 1, GPU lost across down and up | NVIDIA runtime power management suspends the GPU between two labs and the resume fails (Xid 119). | B | Not a driver matter | Already in `next`: `4e47b765` refuses a GPU with `runtime_status=error`, warns on `power/control=auto`, and the supervisor holds the GPU awake (ADR-0018). |
| #133 item 2, console streaks | The same fault as #134. | A | No | See #134 |
| #133 items 3 and 4 | A share without the execute right, and `expect` that dropped the `Err` text. Not device problems. | none | | Already in `next`: the `acl allow execute always` rule described in `docs/manual/pages/guide/shares.wcl`, and `288a9018`. |
| #133 item 5 | The template pinned package 0.1.1-alpha. | none | | A pin in vmlab-templates. Not checked here. |
| #134, stale bands | The flip is not ordered after the rendering that it shows (F7, F10). Which of two orderings fails is open: H1 and H2 below. | A | No | No. Needs C7. |
| #135, WebGL static vertex buffers | The user-mode driver copies into the CPU shadow of a stream-output buffer and never uploads it. | C | Yes, Mesa `76c095bb` | Yes: rebase to November (C1), or the small patch in the card. |
| #136, 64-byte holes | On an Intel CPU host with an NVIDIA GPU, a guest CPU write to uncached host-visible memory (NVIDIA type 2) does not reach the GPU, and the reverse (`guest/win3d/docs/nvfb.md`). Patches 0065 to 0067 cover the Venus ICD. The Yttrium Direct3D transport picks its memory type itself and still takes type 2. Which allocation does so is a candidate in the card, not a finding. | B | No, the transport reads the same in November | Yes: hide the type on the host (the layer in the card), or prefer cached memory in the transport. C3. |
| #137, pointer lag | The kernel driver reports no hardware pointer (F8), so Windows draws the pointer into the frame. | A | No | Yes: the patch in the card. The host side needs nothing (section 1, scanout row). C2. |
| #138, gaps in Direct2D shapes | The driver draws target-independent rasterization with a fragment shader interlock. On NVIDIA through Venus two fragments of one pixel with disjoint coverage lose an XOR. Whether NVIDIA's driver or the renderer loses the lock is not known: no AMD or Intel comparison exists. | B, suspected. Not C. | No | Yes: the patch in the card (one pass per sample, no lock). C4. |
| #139, Desktop Duplication | Three gaps: the kernel driver refuses a present copy into a blob (F9), the duplication surface is device-local and cannot be mapped, and the first frame is cleared. | A | No, the check is still in `e9917ae2` | Yes: the two patches in the card. C5. |
| #140, Chromium GPU process crashes | There is no Direct3D 12 driver, so the package replaces `d3d12.dll` with a file that Microsoft did not sign. A Chromium GPU process refuses it when it loads it late, and ends itself. | A | Partly: the INF now has a Direct3D 12 driver slot, with no source that I could find | Yes, as a workaround: the preload patch in the card. C6. |

The two hypotheses for #134. The issue shows that the frame is already torn
when the guest flips it, that only GPU-drawn content tears, and that it tears
more when the host is slow.

- H1: DWM composes before the application's rendering has finished on the
  host. On a real card both submissions run on one engine in the order of the
  Windows scheduler. Here they are two host queues with no dependency between
  them (F10), and NVIDIA dma-bufs carry no implicit fence. If the driver tells
  Windows that an application's submission is complete when the host has only
  received it, Windows hands the surface to DWM too early.
- H2: The flip leaves before DWM's composition has finished on the host (F7).
  The issue's re-read probe makes H2 less likely than H1, on QEMU.

Both are the same missing property (P5 below): completion that the guest
sees must mean completion on the host GPU.

## 3. What "acts like a real video card" means

The properties that Windows, DWM and applications rely on, and what the
device has today.

| | Property | Today | Problems |
| --- | --- | --- | --- |
| P1 | A hardware pointer: the driver takes the shape and the position, and the pointer is not part of the frame. | No (F8). The host can already show one. | #137 |
| P2 | A vblank that belongs to the display: an interrupt per shown frame, a scan line, flip timestamps. | A guest timer (F6). | Pacing limits in `known-limits.md` |
| P3 | A flip shows a finished frame. | No ordering (F7). | #134 |
| P4 | One timeline for all GPU work: what the scheduler reports as complete is complete, and work of two processes on one surface is ordered. | No (F10). | #134 |
| P5 | Completion is truthful end to end. P3 and P4 together. | No | #134 |
| P6 | Present of the Blt kind on the GPU, into any surface that Windows names. | CPU copy, GDI destinations only (F9). | #139, slow Direct3D 9 and DirectDraw presents |
| P7 | Desktop Duplication works. | No. Needs P6 and mappable shared surfaces. | #139 |
| P8 | A driver for every API, registered where Windows looks for it. No system file is replaced. | Direct3D 9 to 11, OpenGL and Vulkan: yes. Direct3D 12: no. Video decode and encode: no. | #140 |
| P9 | Memory that the driver calls CPU-visible is coherent between CPU and GPU on every host. | Depends on the host CPU and GPU. | #136, the nvfb faults |
| P10 | Rendering is correct for the feature level that the driver reports. A vendor proves this with conformance tests. | Five probes, a few applications, no conformance run of ours. Upstream says it passes the Wine Direct3D tests. | #135, #138 |
| P11 | The adapter survives power changes and loss of the device: timeout recovery, stop and start, a lost host GPU reported as device removed. | Timeout recovery is in the 0.1.3 candidate (kmd 0004). A lost host GPU ends the hypervisor. Hibernation is off. Online snapshots are refused. | #133 item 1 on the guest side |
| P12 | A production signature. Secure Boot stays on and no Test Mode. | Test-signed. | Decision 0001. Also the first-launch Defender delay. |
| P13 | Display topology: several outputs, hot-plug, modes from EDID, more than 60 Hz. | One output at 60 Hz. vmlab-hv has an EDID and a config change event. | None of the eight |
| P14 | The adapter works in session 0. | Vulkan does not enumerate for SYSTEM. | Test tooling only |

P1 to P7 and P13 are the display half of an adapter. P8, P10 and P12 are the
driver package. P9 and P11 are the host. No single change of the device
model reaches all three groups.

## 4. Ways to get there

### O1: complete the current driver, in the guest only

Keep the device as it is. Add what is missing as patches on the Yttrium
kernel driver and Mesa fork: the pointer, the two waits for #134 if a guest
wait is enough, Desktop Duplication, the per-sample passes, the Direct3D 12
preload. Rebase to the November release.

- Fixes: #135, #136, #137, #138, #139. #140 as a workaround. #134 only if H1
  and H2 can both be closed with fences that virtio-gpu already has (a fenced
  command on a context ring).
- Does not fix: P2 (the vblank stays a guest timer), P6 (present stays a CPU
  copy), P8, P12, P13.
- Cost, in order: rebase (C1), then the patches of C2 to C6 in any order,
  then the #134 investigation (C7).
- R1: The patch stack grows (51 patches today) on an upstream that publishes
  one squashed commit a month. Each release is a rebase of everything.
- R2: The kernel driver build has no tracing (`known-limits.md`, "Kernel
  driver diagnostics"). Work on flip ordering without it is guesswork.
- Tests needed: the adapter suite of C8, on AMD1 and Nvidia2 on helios, and
  an Intel CPU host with an NVIDIA GPU for #136.

### O2: O1, and extend the vmlab-hv device where only the host knows

The same guest work, and a small private extension of the virtio-gpu device
in vmlab-hv behind one feature bit, used only by our kernel driver. Linux
guests and QEMU hosts do not see it. Candidates, each taken only when O1's
guest-only form is measured as not enough:

- A flip command that names the frame and is answered when the host has
  published it, after the host has waited for the rendering it depends on.
  The host is the only place that sees every queue (P3 to P5).
- A vblank event from the frame clock of the host scanout with a host
  timestamp, in place of the guest timer and its empty command per tick (P2).
- A copy between two resources on the host GPU, for present of the Blt kind
  and for Desktop Duplication (P6, P7).
- A device-lost event, so that a lost host GPU is a removed device in the
  guest and not a dead VM (P11).
- A host-side memory policy: the host hides memory types that are not
  coherent on this host (P9). This one is a layer in the render server and
  needs no feature bit. It is card C3.

- Fixes: everything O1 fixes, #134 by construction if the guest-only waits
  fail, and P2, P6, P11.
- Does not fix: P8 and P12. Those are Q3 and Q4.
- Cost: O1, then one card for the feature negotiation and the flip command,
  then one card per further candidate. vmlab-hv is in this repository, so
  device and driver change in one branch.
- R3: The kernel driver's display path diverges from upstream. This is a
  reason to settle Q2 early.
- R4: The extension is off the virtio specification. Mitigation: one feature
  bit, and the driver keeps the standard path when the bit is absent, which
  is also what keeps QEMU hosts working.
- Tests needed: as O1, and the tearing probe of #134 as a regression test in
  the suite.

### O3: split display from render with an indirect display driver

Make the Venus adapter render-only for display purposes and add a user-mode
indirect display driver (IddCx). Windows hands that driver finished desktop
frames with dirty rectangles, a hardware pointer, modes, hot-plug and several
monitors. The driver passes each frame's host resource to the scanout.

- Would give: P1, P3 for the desktop, P13, in user mode where we can debug.
  It removes the dependency on the kernel driver's VidPN code.
- Does not fix: #135, #136, #138, #140. H1 stays, because DWM still composes
  application surfaces on the Venus adapter. Present of the Blt kind stays in
  the kernel driver.
- Cost: a new UMDF driver, a frame hand-off to the host, changes to the
  installer and signing for a second driver. One spike card first.
- R5: Not tested by anyone here. The frame hand-off uses shared surfaces of
  the Yttrium Direct3D 11 driver, the same area where #139 found gaps. The
  cost of the extra copy, if one is needed, is unknown.
- I do not recommend it now. It becomes interesting if C7 shows that the
  kernel driver's flip path cannot be made correct.

### O4: a device model and a WDDM driver of our own

A new PCI device and a kernel driver written by us, with our own display
engine and scheduler model, and in time our own user-mode drivers.

- Would give: P1 to P7, P11 and P13 by design, and full control.
- Does not fix by itself: #135, #138 and #140, unless we also replace the
  Yttrium user-mode driver, which is the largest part of the stack. P9 is a
  host matter in any model.
- Cost: a kernel driver of the size of the Yttrium one (its display and
  memory code is the result of over a year of upstream work), a new transport
  in every user-mode driver, and all of the application fixes in
  `guest/win3d/patches/mesa` done again. Many months before it matches what
  runs today.
- R6: The four class A problems return as our own faults during the rewrite,
  and the non-class-A half of the problems is not touched.
- I do not recommend it.

### O5: hand the guest a real GPU

vmlab-hv already takes a pre-bound vfio device. The guest then runs the
vendor's driver and none of the eight problems exist. It needs one GPU per
VM, it has no console through vmlab without more work, and laptops cannot do
it. It is a mode for a host with a spare card, not a replacement for a
shared GPU. It needs no card from this design.

### Comparison

| | #134 | #135 | #136 | #137 | #138 | #139 | #140 | Size |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| O1 | maybe | yes | yes | yes | yes | yes | workaround | 9 cards |
| O2 | yes | yes | yes | yes | yes | yes | workaround | O1 plus 2 to 5 cards |
| O3 | partly | no | no | yes | no | no | no | a new driver |
| O4 | yes | no | no | yes | no | yes | no | many months |
| O5 | yes | yes | yes | yes | yes | yes | yes | one GPU per VM |

## 5. Recommendation

Take O2, in stages.

- Stage 1 is O1: the rebase, the six point fixes, the #134 investigation
  with a traced kernel driver, and the adapter suite. It ends seven of the
  eight cards and decides what stage 2 needs. Every card of stage 1 is useful
  whichever option the operator later takes, except under O4.
- Stage 2 is the device extension, starting with the flip command if C7
  shows that guest waits do not end #134, then the host vblank, then the host
  copy for presents.
- Stage 3 is the driver package: signing (Q3) and Direct3D 12 (Q4).

Reasons: O2 is the smallest change that reaches P1 to P7. It keeps the
application fixes we already have. The device is ours to change since
vmlab-hv is in tree. And the half of the problems that is not about the
device model (F1) is answered by the test suite and the host layer, which no
device model would give.

### Point fixes that can be done now

- #133: nothing left to fix for items 1, 3 and 4. Close it against #134 once
  the template pin of item 5 is checked.
- #135: C1, or the card's patch if the rebase is put off.
- #136: C3.
- #137: C2.
- #138: C4.
- #139: C5.
- #140: C6, as a workaround.

## Cards

Stage 1. C1 goes first, because the patches in the issues were written
against `45ec46ca` and each must be rebased anyway. C2 to C6 and C8 are
independent of each other after C1. A card that changes the package ends with
a package build and the win3d e2e run (`guest/win3d/tests/e2e/run.sh`) on
AMD1 and Nvidia2 on helios, 12 of 12 on each.

- **C1. win3d: rebase the driver package to the Yttrium November release.**
  Do: move `build/kmd/PINS` to `e9917ae2` and `build/mesa/PINS` to
  `76c095bb`, rebase the 4 kernel driver and 47 Mesa patches, drop what
  upstream now has, and decide what to do with the new `yttrium12.dll` INF
  entry (leave it out unless its binary can be built from source). Done: the
  package builds, the e2e run passes on both GPUs, the repro page of #135
  reads `255,0,0,255` on all three lines, and `stack.md` tables are
  regenerated. Ends #135.
- **C2. win3d: hardware pointer in viogpu3d.** Do: carry the patch of #137
  as a kernel driver patch, with `HWCursor` on in our INF, and check the
  pointer on RFB, in the viewer and in a recording on vmlab-hv. Keep the
  documented limits (64 pixels, inverted pixels drawn as an outline). Done: a
  screenshot of the frame without the pointer and the cursor shape present on
  RFB, pointer shapes arrow, I-beam, resize and hidden all arrive, and a
  check in C8's suite. Ends #137.
- **C3. win3d: keep Venus guests out of host memory that is not coherent.**
  Do: on the host, hide an uncached host-visible memory type that has a
  cached counterpart in the same heap from the render server, as the layer in
  #136 does. vmlab already loads its own Vulkan layer into the renderer
  (`src/qemu/vk_pci_filter.c`), so this can be a second rule there with a
  host GPU setting. Decide by measurement whether it is on for every NVIDIA
  host or only where the probe of patch 0067 fails. Done: on an Intel CPU
  host with an NVIDIA GPU the dash count of #136 is 0 in two boots, and the
  e2e run on helios is unchanged in pass count and frame rate. Ends #136.
  Needs Q5.
- **C4. win3d: draw forced-sample-count coverage without the shader
  interlock.** Do: carry the patch of #138, and run the same Word start on
  AMD1 without the patch to learn whether the fault is NVIDIA's (class B) or
  the driver's. Done: three starts of Word give the same ribbon to the pixel
  on Nvidia2, the AMD result is written into `known-limits.md`, and a
  Direct2D probe is in C8's suite. Ends #138.
- **C5. win3d: Desktop Duplication.** Do: carry the two patches of #139,
  narrow the texture pattern if a better signal is found, and add the
  eight-frame test as a probe. Done: `AcquireNextFrame` returns lit frames on
  AMD1 and Nvidia2, the shared-texture test passes, and `present.md` no
  longer says that duplication fails. Ends #139.
- **C6. win3d: keep Chromium's GPU process alive with the Direct3D 12
  bridge installed.** Do: carry the preload patch of #140, install the
  native copy and renew it in the repair task, and add `venc.html` as a
  probe. Done: Chrome and Edge keep WebGL for 20 s after the encoder query,
  with no new crash dump, and a native `D3D12CreateDevice` still gets the
  bridge. Ends #140 as a workaround. Its real end is Q4.
- **C7. win3d: find and fix the flip ordering on vmlab-hv.** Do, in this
  order: build a kernel driver with tracing (a `DBG` build, see
  `build/kmd/README.md`). Repeat the WPF repro and the RFB grabber of #134 on
  vmlab-hv on Nvidia2 and AMD1, with and without host load. Test H1 and H2:
  hold an application's completion until a fence on its host queue has
  signalled, and hold the flip until a fence on DWM's queue has signalled.
  Done: either 0 torn frames in 20,000 at the 90 percent load of the issue,
  with the patch that did it, or a written result that says which wait is
  missing and that the host must order the flip, which starts stage 2. Ends
  #134 in the first case, and #133.
- **C8. win3d: an adapter test suite in the acceptance run.** Do: one
  script that runs in the e2e lab and checks the properties of section 3 that
  can be checked: pointer not in the frame (P1), vblank interval and present
  rate (P2), the tearing probe (P3 to P5), Desktop Duplication (P7), the
  static vertex buffer page and `venc.html` in Chrome (P8, P10), the Direct2D
  coverage probe (P10). Add a run of the Wine Direct3D 9, 10 and 11 tests and
  record the baseline. Done: the suite runs with `--apps`, each check prints
  pass, fail or not-run, and the baseline for AMD1 and Nvidia2 is in
  `acceptance.md`. Ends none. It is what tells us about the next #135 before
  a user does.
- **C9. win3d: rebuild the `#win3d` template variants on the fixed
  package.** Do: after C1 to C6, cut a package release, move the pin in
  vmlab-templates, and build the variant for the newest Windows 10 and 11
  template versions (the newest have none today). Done: both variants are in
  the store and the registry, and the e2e run passes on a clone. Ends #133
  item 5.

Stage 2, one card each, to be written after C7 reports:

- **C10. hv, win3d: a feature bit and a host-ordered flip command.** Only
  if C7 ends without a guest-only fix.
- **C11. hv, win3d: vblank from the host frame clock.**
- **C12. hv, win3d: present copies on the host GPU.**
- **C13. hv, win3d: report a lost host GPU to the guest as a removed
  device.**

## Decisions for the operator

- Q1. The way: O2 as recommended, or O1 only, or a spike of O3, or O4.
- Q2. The upstream driver. Today we carry 51 patches on a single-author
  alpha that ships one squashed commit a month. Choices: send our fixes
  upstream and stay close, or keep carrying, or fork the kernel driver for
  good. O2's device extension makes this more pressing (R3).
- Q3. Signing. Attestation signing needs an EV certificate and a Partner
  Center hardware account. It removes Test Mode, lets Secure Boot stay on and
  avoids the Defender delay at first launch. Without it the device cannot be
  called a real video card for a user (P12). This is money and an account,
  not engineering.
- Q4. Direct3D 12. A native Direct3D 12 driver is the only real end of #140
  and of the replaced system file (P8). It is a large piece of work: the
  Direct3D 12 driver interface in front of vkd3d-proton, or a move to whatever
  upstream is doing with `yttrium12.dll`, whose source I did not find. The
  alternative is to accept the bridge with C6.
- Q5. Hardware for tests. #136 and #133 item 1 only happen on an Intel CPU
  host with an NVIDIA laptop GPU. helios cannot show them. C3 needs such a
  machine (Zeus is the one the documents name) or a reporter who runs a test
  build.
- Q6. QEMU hosts. The device extension of O2 exists on vmlab-hv only. If
  QEMU stays a supported host for `#win3d` guests, those hosts keep the O1
  behaviour and no more.
