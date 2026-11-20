# Presentation: slow D3D9 and DirectDraw presents, translucent GL windows

This page explains three presentation bugs from
[`known-limits.md`](known-limits.md), the fixes, and the evidence:

1. D3D9 ran at about 30 fps on the AMD host, against about 60 for D3D11,
   D3D12 and Vulkan, and 59 fps on the Intel host.
2. DirectDraw windowed blits to the primary reached 13 to 17 per second.
3. Blender's window was translucent on Windows 10.

The fixes are `patches/kmd/0002`, `patches/mesa/0026` and
`patches/mesa/0027`. All three are in the package from commit [12be6fd](https://github.com/VMLabDev/vmlab-win3d/commit/12be6fd7969a64a93a0ce60207edd9522a6a3049).

helios: QEMU 11.1.1, virglrenderer 1.3.0, RADV (Mesa 26.2.2), NVIDIA
610.57.04, vmlab 0.28.0-alpha. The AMD runs used the system QEMU. The
`Nvidia1` and `Nvidia2` runs used the egl-headless QEMU at
`/home/wil/.local/opt/qemu-11.1.1-vmlab-egl` (`qemu-argv-env.txt` in each
evidence directory).

## Results

| Run (package [12be6fd](https://github.com/VMLabDev/vmlab-win3d/commit/12be6fd7969a64a93a0ce60207edd9522a6a3049)) | `tests/e2e/run.sh` | D3D9 probe x64 / x86, before | after | Repro (`pace`, `gl_alpha`) |
| --- | --- | --- | --- | --- |
| Windows 11, `AMD1` | 12 of 12 | 31.5 / 31.1 fps | 59.4 / 58.2 fps | 14 of 14 cases pass (x64 and x86, plus 1280×720) |
| Windows 11, `Nvidia1` | 12 of 12 | 22.3 / 21.8 fps ([acceptance run](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/acceptance/windows-11-nvidia1/results.txt)) | 39.2 / 38.2 fps | 11 of 14 pass: x64 D3D9 and D3D9Ex at interval one reach 47.6 and 48.2 per second, under the repro's 50, and 37.9 at 1280×720 |
| Windows 10, `Nvidia2`, with `--apps` | 12 of 12 | 18.98 / 22.94 fps (`Nvidia1`, [acceptance run](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/acceptance/windows-10-nvidia1/results.txt)) | 49.5 / 50.1 fps | D3D9 52.6 and 51.0, DirectDraw 56.6 and 56.9 per second (x64, x86); Blender opaque |

The "before" AMD numbers are from the same e2e run on the parent commit
[b4b8c3f](https://github.com/VMLabDev/vmlab-win3d/commit/b4b8c3fada1c47d2079ce9ab4699fa8c544a8717) ([docs/evidence/present/before/amd1/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/present/before/amd1/)). The repro reports
for each case are in [docs/evidence/present/after/<gpu>/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/present/after/).

| Case, `AMD1`, x64, 640×480 | Before: per second, ms in each call | After |
| --- | --- | --- |
| D3D9, interval one | 45.5, 21.1 ms | 59.5, vblank-paced |
| D3D9, interval immediate | 59.2, 16.3 ms | 561.8 |
| D3D9Ex, interval one | 48.6, 19.8 ms | 59.4 |
| DirectDraw Blt to the primary | 34.0, 29.2 ms | 672.2 |
| D3D9, interval one, 1280×720 | 19.3 (immediate), 48.7 ms | 59.5 |
| DirectDraw, 1280×720 | 10.4, 95.3 ms | 148.5 |
| KMD vblank interval | 16.67 ms | 16.67 ms |

## Cause of bugs 1 and 2: the KMD's CPU copy wrote uncached memory

A windowed D3D9 Present and a DirectDraw Blt to the primary both reach the
KMD as a **Blt present**: `pfnPresentCb` with a source and a destination
allocation. On a Venus context the Yttrium KMD carries a Blt present out
on the CPU (`VioGpuDevice::Present` → `SetCpuCopyBlt` and `MapCpuCopyBlt`,
`VioGpuCpuCopyBltPresent` in `viogpu_command.cpp`). It copies the source
blob's mapping into the destination row by row. When the formats differ,
it copies pixel by pixel to swap red and blue. The destination is never a
blob (`VioGpuCpuCopyBltPresent` refuses one). It is a GDI shadow or staging
surface that dxgkrnl creates through `GetStandardAllocationDriverData`: a
guest-RAM allocation in segment 1.

Both ends of that copy were mapped without the CPU cache:

- The KMD created segment 1 allocations without `Flags.Cached`. VidMm
  therefore mapped the destination write-combined. The segment is declared
  `CacheCoherent`, which is the precondition for `Cached`, and the host
  reaches guest RAM with the CPU.
- The source is a Venus blob. The KMD maps a blob with the cache type the
  host reports. virglrenderer reports device-local, host-visible memory as
  write-combined. The Yttrium source has a comment about the same effect on
  CPU reads (`viogpu_allocation.cpp`, "a bulk copy … reached only
  ~21 MB/s"). D3D10/11 back buffers and D3D9 `DiscardRenderTarget`
  resources already ask for host-cached memory for this reason. DirectDraw's
  off-screen plain surfaces did not.

**Why the host CPU mattered.** On an AMD CPU, KVM's nested paging keeps the
guest's memory type, so the guest's write-combined or uncached mapping
really is one. On an Intel CPU, KVM's EPT maps guest RAM write-back and
ignores the guest's memory type, unless a device does non-coherent DMA. The
same guest code was fast on Zeus (Intel, 59 fps) and slow on helios
(AMD Ryzen 9 9950X), whichever GPU helios used: the D3D9 probe ran at about
30 fps on the AMD iGPU and about 20 fps on the RTX 5090.

**How the cause was found** ([docs/evidence/present/before/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/present/before/),
`isolation/`):

1. The KMD's vblank timer was steady at 16.67 ms in every run, so the
   60 Hz timer was not the cause.
2. With `D3D10UMD_YTTRIUM_PRESENT_EVERY=0`, the UMD skips `pfnPresentCb`.
   D3D9 then ran at 1023 presents per second, so rendering and publication
   cost about 1 ms (`isolation/present-every-0/`).
3. The time in each Present scaled with the window area: 3.0 ms at
   160×120, 16.3 ms at 640×480 and 48.7 ms at 1280×720. DirectDraw took
   5.7, 29.2 and 95.3 ms (`before/amd1/size/`). That is a copy at about
   75 MB/s, not a wait for vblank.
4. During a 1280×720 run, the presenting process used 9.06 s of CPU in
   10 s, 8.95 s of it in kernel mode. DWM used 0.89 s
   (`isolation/cpu-samples.txt`). The copy runs in the presenting thread,
   inside `pfnPresentCb`.
5. With the source in host-cached memory alone (a first draft of 0026 that
   also covered D3D9 back buffers), DirectDraw improved and D3D9 did not
   (`isolation/mesa-only/`). With KMD 0002 alone, D3D9 reached
   57.6 per second at interval one and 505 at immediate, while DirectDraw
   stayed at 26 per second at 1280×720 (`isolation/kmd-only/`). Both
   together fixed both (`isolation/kmd-and-mesa/`).

At interval one, the D3D9 runtime waits for a vblank and then presents.
With a 16 ms Present, every frame crossed a vblank, which is where 30 fps
came from. The repro's lighter frame lost fewer vblanks (45.5 fps).

## Fixes for bugs 1 and 2

**`patches/kmd/0002-kmd-cache-gdi-shadow-and-staging-surfaces.patch`**
sets `Flags.Cached` on the KMD's shadow and staging surfaces. The KMD
already tags exactly these two standard allocation types with
`VIRGL_RESOURCE_FLAG_MAP_COHERENT`, and the patch uses that tag. The
brief's rule is not to change the KMD unless the cause is there. Here it
is: the KMD creates these allocations, and only the KMD can choose their
CPU mapping. No UMD change can make the destination of a Blt present
cached, and the D3D9 swapchain needed no UMD change once the destination
was cached.

A broader first attempt set `Cached` on every non-blob allocation. That
broke the stack: no Venus device, D3D9 `CreateDevice` failed with
`D3DERR_NOTAVAILABLE`, and OpenGL fell back to GDI. Only 3 of 12 checks
passed (`isolation/kmd-all-cached/e2e-results.txt`). The reason was not
investigated, because the release KMD has no tracing. Some non-blob
allocation besides the GDI surfaces must not be mapped cached.

**`patches/mesa/0026-d3d9-ddraw-present-sources-in-host-cached-memory.patch`**
puts DirectDraw present sources in host-cached memory (UMD
`YTTRIUM_GDI_RESOURCE_FLAG_CPU_READBACK`). Patch 0014 identifies these
surfaces as Blt sources.

## Cause and fix of bug 3: the WGL swapchain asked for premultiplied alpha

OpenGL presents through Zink's Kopper swapchain on the Win32 Vulkan WSI.
The WSI builds that swapchain as a DXGI composition swapchain, attached to
the window through DirectComposition. Mesa's WGL frontend set `has_alpha`
for every window (`stw_st_fill_private_loader_data` in `stw_st.c`). Kopper
therefore asked for `VK_COMPOSITE_ALPHA_PRE_MULTIPLIED_BIT_KHR`, and the
WSI created the swapchain with `DXGI_ALPHA_MODE_PREMULTIPLIED`. The
compositor then blended the alpha left in the back buffer with whatever
was behind the window. The OpenGL probe writes alpha 1 and was opaque.
Blender leaves alpha below 1 in its UI.

**`patches/mesa/0027-wgl-present-windows-opaque.patch`** clears
`has_alpha`. The swapchain becomes `VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR`,
which the WSI maps to `DXGI_ALPHA_MODE_IGNORE`. That matches native
Windows: GDI windows and DWM redirection surfaces ignore a GL
framebuffer's alpha.

An application that asks DWM for per-pixel transparency
(`DwmEnableBlurBehindWindow` with an alpha pixel format) is not detected,
and its window is now opaque. DWM offers no query for blur-behind. No
application in the test set does this.

## Windows 10 and Blender

The Windows 10 run was `tests/e2e/run.sh --template windows-10 --host-gpu
Nvidia2 --keep --apps` on the package from `12be6fd`
([docs/evidence/present/after/windows-10-nvidia2/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/present/after/windows-10-nvidia2/)). It passed 12 of 12,
and Blender 4.2.3 was opaque (`console-blender.png`).

The earlier translucent capture was on `AMD1`. To separate the fix from the
host GPU, Blender ran again in the same lab with each WGL DLL, through
`swap-umd.sh`:

- with the DLL from before 0027 ([b4b8c3f](https://github.com/VMLabDev/vmlab-win3d/commit/b4b8c3fada1c47d2079ce9ab4699fa8c544a8717)), the wallpaper shows through
  the viewport and panels (`before-0027/console-blender.png`);
- with the packaged DLL, the window is opaque
  (`console-blender-same-lab.png`).

`gl_alpha` did not reproduce the fault. Its window was opaque with the old
DLL as well, in a pop-up window and in an overlapped window with
`PFD_SUPPORT_COMPOSITION` (`before-0027/gl_alpha-*.txt` and the console
captures). What Blender does differently was not found. `gl_alpha` stays in
the repro as a check that an alpha-0 GL window is opaque, and the Blender
A/B above is the reproduction of bug 3.

A second attempt ([docs/evidence/hostvirgl/gl_alpha/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/hostvirgl/gl_alpha/)) had GDI paint the
GL window's own redirection surface magenta as well, and judged by
console captures every 3 s instead of `GetPixel` alone. With a WGL built
without 0027, the window was still opaque on Windows 10 (`AMD1`) and
Windows 11 (`Nvidia1`), at alpha 0 and 0.5. DXGI Desktop Duplication was not
tried as an in-guest reading of DWM's output: it gave no frames at that time
([Desktop Duplication](#desktop-duplication)).

## The repro

[docs/evidence/present/src/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/present/src/) has two probes, built for x64 and x86 by
`src/build.sh` in the project toolchain image:

- `pace.exe <mode> <seconds> <out> [width height]` runs one present
  loop: `d3d9` (windowed, DISCARD, interval one, as `probes/d3d9`),
  `d3d9-imm`, `d3d9ex`, `ddraw` (DDSCL_NORMAL, clipper, `Blt` from a
  video-memory surface to the primary) or `vblank`. A second thread waits
  on `D3DKMTWaitForVerticalBlankEvent` throughout. The report gives calls
  per second and the milliseconds in each call, per frame and per vblank.
  A rendering case passes at 50 calls per second or more.
- `gl_alpha.exe <seconds> <out> [alpha]` puts a GL window, with an 8-bit
  alpha pixel format and cleared to alpha 0, over a magenta window. It then
  reads the composed desktop at its centre with `GetPixel`. It passes when
  every sample is the clear colour. GDI paints the GL window's own
  background magenta first. It passed before 0027 as well, so it
  guards the opaque case but does not reproduce bug 3 (see
  [Windows 10](#windows-10-and-blender)).

`docs/evidence/present/run-pace.sh <lab-dir> <out-dir> <arch> <case>
[seconds [width height]]` runs one case in the autologon desktop of a lab
kept by `tests/e2e/run.sh --keep`. [docs/evidence/present/swap-umd.sh](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/present/swap-umd.sh)
replaces the installed UMD and WGL DLLs in such a lab for development. The
isolation runs used it. The proof runs in the results table are clean
installs.

## Desktop Duplication

A program that captured the screen with `IDXGIOutput1::DuplicateOutput` got
no picture (GitHub #237): `DuplicateOutput` succeeded and every
`AcquireNextFrame` returned `DXGI_ERROR_ACCESS_LOST` (`0x887A0026`). Screen
recorders and remote-desktop tools showed black. Since driver
`100.6.101.58107` (`patches/kmd/0006` and `patches/mesa/0095`) it delivers
frames.

**How a frame gets there.** dxgi creates the duplication surface in the
capturing program. For each frame, DWM's context has dxgkrnl copy the desktop
into that surface through the kernel driver's `DxgkDdiPresent` with `Blt`. A
trace of `Microsoft-Windows-DxgKrnl` showed that copy failing:
`OutputDuplicationBlit … Status=0xC00000BB` (`STATUS_NOT_SUPPORTED`) from
dwm.exe, then `Dxgk_Diag_OutputDupl_Destroy`.

**Three things were in the way.**

1. The kernel driver refused the copy. In a Venus context it does a present
   copy on the CPU, and only from a Venus image into a GDI surface
   (`SetCpuCopyBlt` and `VioGpuCpuCopyBltPresent`: `!src->IsBlob() ||
   dst->IsBlob()`). The duplication surface is a blob.
2. The surface could not be mapped. Since the November release the
   user-mode driver creates a shared render target as an "optimal shared"
   image: optimal tiling in device-local memory, a blob that is not mappable,
   and no row pitch. A CPU copy has nowhere to write.
3. The first frame was thrown away. The user-mode driver clears an image
   nothing was drawn to at its first use (`contents_initialized`), and the
   image starts in `VK_IMAGE_LAYOUT_UNDEFINED`. The kernel's first copy, the
   only one of the whole desktop, comes before that. With 1 and 2 fixed
   alone, the first frame is black and the later ones carry only what
   changed, on black.

**The fixes.**

- `patches/kmd/0006`: a present copy into a blob is allowed when source and
  destination are both mappable blobs. The destination is mapped in
  `Present`, where a refusal fails only that call, and written at the image's
  offset in the blob with the row pitch of the allocation.
- `patches/mesa/0095`: a texture that looks like a duplication surface is
  created as a display image with the winsys flag
  `YTTRIUM_GDI_RESOURCE_FLAG_KERNEL_WRITTEN`: linear, in cached host-visible
  memory, a mappable blob, with its row pitch and offset in the allocation
  record the kernel driver reads. It is created
  `VK_IMAGE_LAYOUT_PREINITIALIZED` and counts as written, as an imported
  display image already does.

**The pattern.** The driver is not told which texture is a duplication
surface. It sees `format=87 bind=0x28 misc=0x2` (BGRA, render target and
shader resource, shared), and the runtime keeps the keyed-mutex and NT-handle
flags to itself. The patch takes a shared `B8G8R8A8_UNORM` 2D texture that is
exactly a render target and a shader resource, default usage, one level, one
slice, one sample, no CPU access, no initial data, and of exactly the size of
the current display mode (`EnumDisplaySettings`, either orientation). The size
is narrower than the pattern in GitHub #237, which took such a texture of any
size: an application's own shared textures keep optimal storage unless they
are as large as the desktop. Such a texture works as before, with linear
tiling, and the `shared-texture` check covers it.

**The patch of GitHub #237 and this one.** The kernel patch is carried as
written. The user-mode patch was written against the September release, where
a shared texture was a Venus image in device-local memory, and only its
first lines reached the board. It was written again for the November
release's storage kinds, with the same three properties (mappable, a row
pitch, not cleared).

**Checks** (the adapter suite, [acceptance.md](acceptance.md#adapter-suite)):

- `duplication`: `dupl_probe.exe` takes eight frames while a window of its
  own changes colour, because Desktop Duplication delivers a frame only when
  something changed.
- `shared-texture`: `share_probe.exe` shares a BGRA render target between two
  Direct3D 11 devices, by legacy handle and with a keyed mutex, at the size
  of the desktop and at 256×256. Each device fills it and the other reads
  every pixel.

**Results.** The runs of 2026-11-20 on helios, the `windows-11` template
(`26100.1742.11`) at 1024×768 on vmlab-hv with the package at commit
`1eeb643e` (driver `100.6.101.58107`): `tests/e2e/run.sh --keep`, then
`tests/e2e/adapter.sh` with the three checks below, on each GPU. The host
kernel logged no Xid.

| Check | AMD1 | Nvidia2 | Before (driver `58106`, [acceptance.md](acceptance.md#baseline)) |
| --- | --- | --- | --- |
| The 12 Done checks of `run.sh` | 12 of 12 | 12 of 12 | 12 of 12 |
| `duplication` | 8 of 8 frames, 786074 of 786432 pixels lit in each, the window's pixel in the colour it had | 8 of 8 frames, 786400 of 786432 pixels lit in each, the window's pixel in the colour it had | 0 of 8 frames, `AcquireNextFrame` answered `0x887a0026` |
| `shared-texture` | 4 of 4 cases, every pixel in both directions | 4 of 4 cases, every pixel in both directions | not in the suite |
| `tear-blt` (the same kernel copy, into a GDI surface) | 0 torn of 1259 console frames, 60.2 presents per second | 0 torn of 1260 console frames, 59.0 presents per second | 0 torn |

Not tried: a rotated output, a second output (the device has one), capture
while a full-screen application flips, protected content. ffmpeg's `ddagrab`
fails earlier and for another reason ("Failed to create D3D11VA device": it
wants a device with video support).

## What remains

- **NVIDIA hosts present D3D9 below the refresh rate.** On `Nvidia1` the
  D3D9 probe rose from about 22 to 39 fps. The repro's D3D9 cases reach 47
  to 55 per second, and 38 per second at 1280×720. There the presenting
  thread is idle: 0.88 s of CPU in 10 s. It waits about 16 to 20 ms in
  each Present whatever the window size, so the cost is not the copy. Host-cached
  back buffers did not change it (`isolation/nvidia1-backbuffers-cached/`),
  so 0026 leaves them device-local. The wait is probably a fence or host
  round trip on the NVIDIA Venus path. It was not investigated further
  within the time box.
- A duplication frame is a CPU copy in the KMD too, of the whole desktop
  for the first frame and of the changed rectangles after it.
- The KMD still copies every Blt present on the CPU. That is now about
  1.5 ms at 640×480 on AMD. A GPU copy would need the destination to be a
  host resource.
- D3D11 and DXGI blt-model swapchains (`DXGI_SWAP_EFFECT_DISCARD`) use the
  same KMD path and should benefit from 0002. Only the flip-model D3D11
  probe was measured.
- The prior art's D3D12 dropped-present and latency findings
  (prior-art §3) are a different path and were not examined.
