# mesa

Builds the Yttrium Mesa drivers for x86_64 and i686 Windows from
`arehnman/virtio-win-mesa` at `76c095bbbd875fda21e55c69d9e1eed56980d956`
(branch `yttrium-experimental`, the November 2026 release, Mesa 26.1.0) plus the patch series in
`patches/mesa/`. The build uses the shared llvm-mingw toolchain image from
`build/toolchain/`.

## Run

```sh
build/mesa/build.sh
```

The script builds or reuses the toolchain image, then runs `inner.sh` in a
container as your uid. Network access is used only for the pinned downloads in
`PINS`, and each run checks their sha256, also when they are cached. Each run
unpacks a fresh source tree, applies the patches, and deletes the build
trees, so a second run starts clean. `MESA_JOBS=<n>` limits Ninja's
parallelism; the default is `nproc`.

## Outputs

```
out/mesa/x64/   viogpu_d3d10.dll  viogpu_wgl.dll  libvulkan_virtio.dll  virtio_icd.json
out/mesa/x86/   (the same four files, 32-bit)
```

| File | What it is | Exports checked |
| --- | --- | --- |
| `viogpu_d3d10.dll` | Yttrium Gallium UMD for D3D9, D3D10 and D3D11, registered through the KMD's `UserModeDriverName[Wow]` | `OpenAdapter` (D3D9), `OpenAdapter10`, `OpenAdapter10_2` |
| `viogpu_wgl.dll` | Mesa WGL ICD with Zink on the Venus ICD, registered as `OpenGLDriverName[Wow]` | `DrvValidateVersion`, `DrvSetCallbackProcs`, `DrvCreateContext`, `DrvSetContext`, `DrvSwapBuffers`, `DrvGetProcAddress`, `DrvDescribePixelFormat`, `DrvSetPixelFormat` and the other `Drv*` WGL ICD entry points |
| `libvulkan_virtio.dll` | Venus Vulkan ICD on the Yttrium KMD (D3DKMT escapes) | `vk_icdGetInstanceProcAddr`, `vk_icdNegotiateLoaderICDInterfaceVersion`, `vk_icdGetPhysicalDeviceProcAddr` |
| `virtio_icd.json` | ICD manifest for the DLL next to it: `library_path` `.\libvulkan_virtio.dll`, `library_arch` 64 or 32, `api_version` as Mesa generates it (1.4.348) | — |

Meson's own `virtio_icd.<arch>.json` assumes a Unix install tree
(`../../../bin/vulkan_virtio.dll`), so the build writes its own manifest. The
Vulkan loader resolves a relative `library_path` against the manifest's
directory, so the manifest and the DLL must be installed in the same directory.

The DLLs link libc++, libunwind and winpthread statically (`-static`). They
import only GDI32, KERNEL32, USER32, `api-ms-win-core-synch-l1-2-0` and the
UCRT `api-ms-win-crt-*` API sets. They are stripped (`--strip-unneeded`), and
the PE timestamp is zero (`--no-insert-timestamp`).

The Meson options are the prior art's (`R/build.sh`, prior-art section 6):
`-Dplatforms=windows -Dgallium-drivers=zink -Dgallium-yttrium=true
-Dgallium-d3d10umd=true -Dgallium-wgl-dll-name=viogpu_wgl
-Dgallium-d3d10-dll-name=viogpu_d3d10 -Dvulkan-drivers=virtio -Dllvm=disabled
-Dzlib=disabled -Dzstd=disabled -Dexpat=disabled -Dbuild-tests=false`.

## Inputs

`PINS` lists every download with its sha256:

- the Mesa source tarball at the commit above;
- `microsoft.windows.sdk.cpp` and `microsoft.windows.wdk.x64` 10.0.26100.1
  from nuget.org. `prepare-winddk.py` extracts the 13 DDK headers the
  D3D10 UMD and Venus need into `include/winddk`. These are the same packages
  the prior art used (identical sha256);
- DirectX-Headers v1.619.1, unpacked as Mesa's `subprojects/DirectX-Headers-1.0`.
  It is the revision of Mesa's own wrap file. Meson runs with
  `--wrap-mode=nodownload`, so it never fetches anything.

## Patches

The patches apply in the order listed in `patches/mesa/series`. The series has two parts.

- **0001 to 0011** come from the prior art. The codex session left its fixes
  uncommitted in `R/mesa`, and its exported `R/mesa-driver.patch` is stale
  (prior-art section 3). These patches were re-exported from
  `git diff 374c3022` of that working tree. It had 43 changed files,
  +1793/−161, and two untracked files. The two untracked files are the
  mixed-case header symlinks that `prepare-build.py` creates, so they are
  build output, not source. The diff was then split by topic. Each patch has
  a header that says what it fixes and which prior-art finding it comes from.
- **0012 to 0017, 0021 to 0024, 0026, 0027, 0030 to 0032, 0035, 0040 to 0044,
  0060, 0065 to 0067, 0070, 0080, 0085, 0086 and 0091 to 0096** are this
  repository's own fixes.

By DLL: 0005 to 0010, 0012 to 0017, 0021 to 0024, 0026, 0030 to 0032, 0035,
0040 to 0044, 0060 and 0091 to 0096 change `viogpu_d3d10.dll`, the
D3D9/D3D10/D3D11 UMD (the `d3d10umd` frontend and the Yttrium winsys). 0032
also changes Gallium's `tgsi_to_nir`, in a branch that only a TGSI
tessellation control shader reaches, which only the UMD makes. 0001 to 0003
are build and utility fixes that every DLL shares. 0004, 0027 and 0080 are in
WGL. 0011, 0065 to 0067, 0070 and 0085 are in the Venus ICD, and 0086 is in
Zink (WGL).

The series was rebased from the September release (`374c3022`) to the
November release on 2026-11-19. "Dropped at the November rebase" below lists
what upstream now has.

### From the prior art

| Patch | Fixes | Prior art |
| --- | --- | --- |
| `build-winddk-includes-and-x86-atomics` | WDK include path for libgallium and Venus. Replaces `InterlockedIncrement64`, which i686 lacks | F1 |
| `util-xmlconfig-case-insensitive-exe-match` | drirc `executable=` matching ignores case on Windows | F2 |
| `util-u_queue-windows-loader-lock-join` | GL shutdown deadlock: queue join under the loader lock | F215–F219 (v15) |
| `wgl-unbind-through-stw-on-delete` | Framebuffer leak when deleting the current GL context | F220–F221 (v16) |
| `yttrium-blend-max-rt` | Garbage blend factors past `max_rt` crashed Heaven | F3 |
| `d3d9-rendering-fixes` | D3D9 sampler type, depth bias, A4R4G4B4, A8L8, D32F, depth uploads | F4, F15, F16, F20 |
| `d3d11-wddm20-ddi` | WDDM 2.0 D3D11 DDI, GPU VA caps, typed UAV loads | F11–F14 |
| `shader-translation-fixes` | Array TXD gradients; raw and typed UAV load swizzles | F17, F28, F29 |
| `yttrium-compute-and-coherence` | Cube/array views, resource waits, staged uploads, compute buffer SRVs, GPU-written constants, native indirect dispatch | F21, F22, F33, F38, F224–F231, staged-draw-0212 |
| `dxgi-blt-scaled-srgb-safe` | DXGI present Blt scaling with sRGB-safe copies | F99–F100 (non-XR part) |
| `venus-win32-nt-memory` | Venus opaque Win32 (NT handle) memory export and import | F69–F71 |

### This repository's fixes

See the [Mesa fixes table](fixes.md) for the patch descriptions.

0021 has no mail header. `scripts/pins-table.py` carries its one-line
description.

### Dropped at the November rebase

Upstream's November release (`76c095bb`) has these fixes itself, so the
series no longer carries them. Each was dropped after reading the upstream
code, not after a test that isolates it:

| Dropped | What upstream has now |
| --- | --- |
| 0020 yttrium-gate-feedback-loop-usage-on-format-support | `yttrium_venus_select_feedback_loop_usage` asks the host for the exact image before it adds `ATTACHMENT_FEEDBACK_LOOP` usage |
| 0025 yttrium-packed-16-bit-formats-match-vulkan | `B5G5R5A1` maps to `A1R5G5B5_UNORM_PACK16` and `B5G6R5` to `R5G6B5_UNORM_PACK16` |
| 0075 wsi-win32-blit-shared-swapchain-when-linear-cannot-render | The Win32 WSI was rewritten: a swapchain image is created from the D3D11 texture's shared descriptor (`create_image_and_memory_from_win32_handle`), which carries its tiling and modifier, so the code 0075 patched is gone |
| 0090 yttrium-d3d9-depth-bias-without-float-representation | The pipeline converts an unscaled depth bias to exact `FORMAT` or `FORCE_UNORM` units when the host has no float representation |
| 0006, depth-bias part and depth-upload part | D3D9 depth bias is set in `D3D9CreateRasterizerState` for triangles only, and buffer-to-image uploads use the depth aspect. The sampler-type, `A4R4G4B4`, `A8L8` and `D32F_LOCKABLE` parts stay |
| 0008, UAV load swizzles | Raw and typed UAV loads apply the source swizzle. The array `TXD` gradient part stays |
| 0009, compute buffer SRVs | Compute shaders sample buffer SRVs through uniform texel buffers. The rest stays |
| 0011, WSI handle import | `wsi_memory_win32_handle_import_info` is gone with the WSI rewrite. NT handle export and import for `VK_KHR_external_memory_win32` stay |

`D3D9Tracef` no longer exists upstream, so 0012 and 0035 lost their trace
lines. 0023 no longer calls 0020's helper: it asks the host with
`yttrium_venus_image_create_flags`, before upstream adds feedback-loop usage.
0022 passes no `VkImageViewUsageCreateInfo` for a view of the shadow image.

Applied in order to the pinned tarball, 0001 to 0011 give exactly the codex
tree minus the parts listed below. This was checked by comparing git tree
hashes.

### Left out

| Codex change | Why it was dropped |
| --- | --- |
| `wsi_common_win32.cpp`: load `dxgn.dll`, `d3d1n.dll` and `dcomn.dll` from `System32\YttriumNative` when that directory exists | An experiment with private renamed presentation DLLs, for a global DXGI replacement. The codex session disabled it in its final tested state (`R/VERDICT.md` line 167). |
| `DxgiFns.cpp`: `dxgi_decode_xr` compute shader, its `R10G10B10_XR_BIAS_A2_UNORM` branch in `_Blt`, `Device::dxgi_xr_decode_shader` and its teardown | Exists only for the XR/10-bit presentation side thread (F97–F100). The scaling and sRGB parts of the same `_Blt` change are kept in 0010. |
| `Shader.cpp`: `D3D10UMD_YTTRIUM_COMPUTE_CAPTURE_DIR` bytecode dump | Diagnostic |
| `yttrium_pipeline.c`: `yttrium_capture_compute_*`, `yttrium_trace_compute_buffers`, `D3D10UMD_YTTRIUM_TRACE_COMPUTE`, `…_CAPTURE_COMPUTE_HASH/ORDINAL`, `…_TRACE_COMPUTE_DATA`, `…_SYNC_COMPUTE`, `…_TRACE_INDIRECT_DRAW` | Diagnostics (F18, F24, F39). All of them default to off. |
| `yttrium_shader.c`: `D3D10UMD_YTTRIUM_SHADER_DUMP_ALL_SUCCESS` | Diagnostic (F17 shader capture) |

The codex tree has no PRESENT_EVERY option in Mesa. No YUY2 code is in the
Mesa tree; that work was in vkd3d-proton. The error-level `mesa_loge` lines in
the Venus NT memory path are failure reports, not diagnostics, so 0011 keeps
them.

Which patches have run in a guest, and where. Every row but the last is a
run on the September base (`374c3022`), and names patches as they were then:

| Run | Mesa patches in the package | Evidence |
| --- | --- | --- |
| Acceptance, Windows 11 and Windows 10, AMD | 0001 to 0014, 0020 | `docs/acceptance.md` |
| D3D11 samples, Windows 11, AMD | 0001 to 0017, 0020 | `docs/d3d11-apps.md` |
| Zeus Intel and the helios AMD regression, Windows 11 | 0001 to 0014, 0020, 0021 | `docs/zeus.md` |
| nvtex proof, Windows 11, `Nvidia1` and `AMD1` (e2e, probes, SDK samples) | 0001 to 0017, 0020 to 0025 | `docs/nvtex.md` |
| ffp proof, Windows 11, `Nvidia2` and `AMD1` (e2e, probes, `d3d9_ffp`, `Text3D`) | 0001 to 0017, 0020 to 0025, 0030, 0040 to 0042 | `docs/ffp.md` |
| ffp2 proof, Windows 11 on `Nvidia2` and `AMD1`, Windows 10 on `Nvidia2` (e2e, `d3d9_ffp2`, `d3d9_ffp`) | 0001 to 0017, 0020 to 0027, 0030, 0035, 0040 to 0044 | `docs/ffp2.md` |
| unigl proof, Windows 11 on `Nvidia1` and `AMD1` (e2e, Unigine Heaven and Valley OpenGL) | 0001 to 0017, 0020 to 0027, 0030, 0035, 0040 to 0044, 0070, 0085, 0086 | `docs/unigl.md` |
| unid3d proof, Windows 11 on `AMD1`, `Nvidia1` and `Nvidia2` (e2e, `d3d9_depthbias`, Unigine D3D9 and D3D11) | 0001 to 0017, 0020 to 0027, 0030 to 0032, 0035, 0040 to 0044, 0090 | `docs/unid3d.md` |
| gzdoom proof, Windows 11 on `AMD1` and `Nvidia1` (e2e, `gzgl`, GZDoom OpenGL and Vulkan) | 0001 to 0017, 0020 to 0027, 0030 to 0032, 0035, 0040 to 0044, 0080; the `Nvidia1` runs add 0070 and 0086, the last e2e on each GPU adds 0070, 0086 and 0090 | `docs/gzdoom.md` |
| November base, Windows 11 on `AMD1` and `Nvidia2` (e2e, and the static vertex buffer page of GitHub #233 in Edge) | 0001 to 0093 (43 patches) | `docs/acceptance.md`, "November base" |
| chromium proof, Windows 11 on `AMD1` and `Nvidia2` (e2e, `tests/e2e/chromium.sh`) | 0001 to 0095 (45 patches) | `docs/chromium.md` |
| chromium proof on the series with Desktop Duplication, Windows 11 on `AMD1` and `Nvidia2` (e2e and the adapter suite, `tests/e2e/run.sh --apps`) | 0001 to 0096 (46 patches; the preload was 0095 in the row above) | `docs/chromium.md`, `docs/acceptance.md` B9 |

The nvtex proof run (2026-10-18) has the full series, 0001 to 0025, built by
`scripts/build.sh` at commit [e428846](https://github.com/VMLabDev/vmlab-win3d/commit/e428846ccddaf01dba7efed9d057518f45c522fc). Before it, no recorded run had 0015
to 0017 together with 0021. The runs cover the code paths that the probes,
dxdiag and the samples reach, not every fix in 0001 to 0011; for the rest,
the evidence is the codex session's own finding, cited in the patch header.

## Reproducibility

The build is bit-reproducible on this host. Three runs gave identical sha256
for all eight outputs:

1. the first run;
2. a second run on top of the first run's leftovers;
3. a run from a fresh `git clone` of the branch at another path, with an empty
   `out/` (so every download was fetched again) and a re-run of
   `build/toolchain/build.sh --force`.

The build sets `SOURCE_DATE_EPOCH=0`, `-ffile-prefix-map=<work>=/mesa` and
`--no-insert-timestamp`, and mounts the checkout at the fixed path `/w`.
No other machine was tried. Toolchain image
`vmlab-win3d/toolchain:ab524e07f89c`, 2026-10-17. These hashes are for the
series as it was then, 0001 to 0011:

```
a417f90c7e82ffda1f88ef8365da2f3cc225bca282b7b320a365fb0b844df262  mesa/x64/libvulkan_virtio.dll
d74ca3299119ebd886b793bca315bc245ab315e30ee4701c9e3bfa9ed704bc44  mesa/x64/viogpu_d3d10.dll
759c38ea072be579f96a2daba3afcd1c5117f1f667ad4eb11f86909882021605  mesa/x64/viogpu_wgl.dll
f67d39fde82a46dfdbb34581493632e164fe6694310ff125c0c180c91283304a  mesa/x64/virtio_icd.json
a4b09b58804984d91719f75fdacdf69c7f9ff7c731f90a5f615be189d5360800  mesa/x86/libvulkan_virtio.dll
f50cf4dd0b1804599bc6c72832bd07faefd93a7a25fe8b4585c07ff393a349e3  mesa/x86/viogpu_d3d10.dll
eebe236cb7a97e2bdd540c9d68709f132e48aa887d39374b649c655df5691b3e  mesa/x86/viogpu_wgl.dll
69e6a7c23226a0c1c854f9b8ac340cedc361ad2db66fe6aa7733c8e060e03151  mesa/x86/virtio_icd.json
```

The full series, 0001 to 0021, built once on 2026-10-17 with the same
toolchain image, changes only `viogpu_d3d10.dll`. The other six files keep
the hashes above, which fits every patch after 0011 being in the UMD.
This build was not repeated, so its bit-reproducibility is not checked:

```
a43c70d4e56407055e438a6fc831f61ad9c9e458d98ef9937c360a3863fca2fb  mesa/x64/viogpu_d3d10.dll
657e89272ed867033f03a71204d39a30b5063f0e4b275b0a7a7ecd428813dd7e  mesa/x86/viogpu_d3d10.dll
```

The November base (`76c095bb`, the 43 patches) built once on 2026-11-19 with
the same toolchain image. That build was not repeated, so its
bit-reproducibility is not checked:

```
faa19d2a05a0ecea652f2bef2a60a05a9e65751688358eba8c4fee1b71061713  mesa/x64/libvulkan_virtio.dll
04c5a9b5382a24d48764db83b6fdc369f18472699d547abbccc4c21bdfb6327c  mesa/x64/viogpu_d3d10.dll
8905445289db3ca0cb530499a1adfae1852b219d077dcc8972342a60a283331b  mesa/x64/viogpu_wgl.dll
f67d39fde82a46dfdbb34581493632e164fe6694310ff125c0c180c91283304a  mesa/x64/virtio_icd.json
55da16b5e6c388bb546d208935693c57a46b875f0d17adc1b324f8b6fed92dd4  mesa/x86/libvulkan_virtio.dll
8f92204488461d1dabb16c6119af49c3341f428abf67e9c4351cbc64b173f8b7  mesa/x86/viogpu_d3d10.dll
dcf70f1b0b2b9bdf194f0d0bbeaaf6a47b27e95eb859bbed8877617ceb263ea4  mesa/x86/viogpu_wgl.dll
69e6a7c23226a0c1c854f9b8ac340cedc361ad2db66fe6aa7733c8e060e03151  mesa/x86/virtio_icd.json
```
