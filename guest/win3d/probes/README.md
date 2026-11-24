# Probes

Small Windows test applications that show whether a graphics API renders, on
which adapter, and through which driver. There is one probe per API, built for
x64 and x86:

| Probe | API path | Adapter selection |
| --- | --- | --- |
| `d3d9_probe.exe` | `Direct3DCreate9Ex` (falls back to `Direct3DCreate9`), HAL device, `vs_2_0`/`ps_2_0` | Default adapter |
| `d3d11_probe.exe` | `D3D11CreateDevice`, feature level 11_0, 10_1 or 10_0, DXGI swap chain (flip discard, else discard) | Default adapter (`D3D_DRIVER_TYPE_HARDWARE`); WARP with `--warp` |
| `d3d12_probe.exe` | `D3D12CreateDevice` at 11_0 minimum, flip-discard swap chain | Default adapter (`NULL`); WARP with `--warp` |
| `opengl_probe.exe` | WGL compatibility context from `wglCreateContext`, GL 1.1 vertex arrays | Whatever `opengl32.dll` loads: the display driver's ICD, else GDI generic |
| `vulkan_probe.exe` | `vulkan-1.dll` loaded at run time, `VK_KHR_win32_surface` swap chain, FIFO | First device that can present, GPUs before CPU devices |

Each probe opens a 1024×640 window titled `vmlab-win3d probe: <API> (<arch>)`
that stays on top. It draws a turning pinwheel, an orbiting square, the API
name in large letters, a line with the architecture and adapter name, and
three marker squares in fixed colours: red at the top left, blue at the top
right and green at the bottom left. The scene is identical in every probe, so
a console screenshot shows at a glance which API is on screen and whether it
is animating.

After the last frame is drawn, and before it is presented, the probe copies
the back buffer to CPU memory with the API's own readback path. It then
compares the centre pixel of each marker with the expected colour.

| API | Readback path |
| --- | --- |
| D3D9 | `GetRenderTargetData` into a system-memory surface |
| D3D11 | `CopyResource` into a staging texture |
| D3D12 | `CopyTextureRegion` into a readback-heap buffer, then a fence wait |
| OpenGL | `glReadPixels` from the back buffer |
| Vulkan | `vkCmdCopyImageToBuffer` from the swap chain image |

The probes import only Windows system DLLs. The C++ runtime is linked
statically, and the build fails when a probe imports anything else (see
`build/probes/check-imports.sh`). HLSL is compiled at run time with
`System32\d3dcompiler_47.dll`, which ships with Windows 10 and 11, so no
DXBC blob is committed. The Vulkan SPIR-V is compiled at build time by the
pinned glslang.

## Command line

```
<api>_probe.exe [--seconds N] [--out FILE] [--width W] [--height H] [--warp]
```

| Option | Default | Meaning |
| --- | --- | --- |
| `--seconds N` | 30 | Render for N seconds, counted from the first frame, then read back and exit. `0` renders one frame. A slow start-up does not shorten the render time (`startup_seconds` below). |
| `--out FILE` | `<probe>.json` in the current directory | Where the JSON result goes. |
| `--width W`, `--height H` | 1024, 640 | Client area size in pixels. At least 320×240. |
| `--warp` | off | D3D11 and D3D12 only: use the WARP software adapter instead of the default adapter. |
| `--help` | | Print usage to the parent console (the probes are GUI programs) and exit 3. |

Frames are paced by vsync (present interval 1, FIFO, `wglSwapIntervalEXT(1)`
when the ICD has it). GDI generic OpenGL has no swap control, so that run is
unthrottled and reports a very high `avg_fps`.

Closing the window or pressing Esc ends the run early. The current frame is
still read back, and `window_closed_early` is set.

| Exit code | Meaning |
| --- | --- |
| 0 | Pass: no API error and every marker matched |
| 1 | API failure: anything in `errors`, including a missing loader or ICD |
| 2 | Readback mismatch: no API error, but a marker differs by more than 2 per channel |
| 3 | Usage error |

A warning does not change the exit code. The D3D9 probe also runs a
self-check of `D3DRS_WRAP0`, `WRAP8` and `WRAP15` before the first frame
([docs/acceptance/C344.md](../docs/acceptance/C344.md)). When a draw of
that check gives the wrong value, the probe records it and still renders,
reads back and presents. On the win3d adapter (PCI vendor `0x1af4`) the
record is an error, so the run exits 1 with its frames and readback in the
result. On any other adapter it is a warning. The Direct3D 9 header
defines `WRAP8` to `WRAP15` for `vs_3_0` attributes with
`D3DDECLUSAGE_TEXCOORD`, yet the native NVIDIA driver 32.0.16.1088 wraps
neither `TEXCOORD8` nor `TEXCOORD15`. On an RTX 5090 passed through to a
Windows 11 guest the probe passes with those two warnings, in x64 and x86
(card #486).

## JSON result

Schema version 1. Example (abridged) from `d3d11_probe.exe` on WARP:

```json
{
  "schema": 1,
  "probe": "d3d11",
  "api": "Direct3D 11",
  "arch": "x64",
  "result": "pass",
  "exit_code": 0,
  "exe": "C:\\win3d\\probes\\x64\\d3d11_probe.exe",
  "pid": 4242,
  "session_id": 1,
  "os_version": "10.0.26100",
  "finished_utc": "2026-10-17T10:00:00Z",
  "args": {"seconds": 30, "warp": false, "width": 1024, "height": 640},
  "adapter": {
    "name": "Microsoft Basic Render Driver",
    "vendor": "Microsoft",
    "vendor_id": "0x1414",
    "device_id": "0x008c",
    "software": true,
    "software_reason": "name contains \"Microsoft Basic Render\""
  },
  "device": {"driver_type": "HARDWARE", "feature_level": "11_0", "swap_effect": "FLIP_DISCARD"},
  "frames_presented": 1790,
  "elapsed_seconds": 30.004,
  "avg_fps": 59.66,
  "startup_seconds": 0.212,
  "window_closed_early": false,
  "readback": {
    "ok": true,
    "tolerance": 2,
    "pixels": [
      {"name": "red_top_left", "x": 48, "y": 48, "expected": [255, 0, 0], "actual": [255, 0, 0], "match": true}
    ]
  },
  "errors": [],
  "warnings": []
}
```

### Top-level fields

| Field | Type | Meaning |
| --- | --- | --- |
| `schema` | int | 1 |
| `probe` | string | `d3d9`, `d3d11`, `d3d12`, `opengl` or `vulkan` |
| `api` | string | Human name of the API |
| `arch` | string | `x64` or `x86` |
| `result` | string | `pass`, `api_failure` or `readback_mismatch` |
| `exit_code` | int | The process exit code, as in the table above |
| `exe`, `pid`, `session_id` | | The process. `session_id` 0 means it ran as a service, where Vulkan does not enumerate |
| `os_version` | string | `major.minor.build` from `RtlGetVersion` |
| `finished_utc` | string | When the result was written |
| `args` | object | The options in effect |
| `adapter` | object | Which device rendered (below) |
| `device` | object | API-specific facts (below) |
| `frames_presented` | int | Successful presents (occluded presents are not counted) |
| `elapsed_seconds` | number | From start-up to the last present |
| `avg_fps` | number | `frames_presented / elapsed_seconds`. It includes start-up, so it understates the steady rate on short runs |
| `startup_seconds` | number | From start-up to the start of the first frame. Absent when no frame started. In a fresh Windows guest the first run of a probe can take tens of seconds here while Microsoft Defender checks new DLLs with its cloud service (`docs/x86d3d12.md`) |
| `window_closed_early` | bool | The window was closed before `--seconds` elapsed |
| `readback.ok` | bool | Every marker was read and matched within `tolerance` |
| `readback.pixels[]` | array | `name`, `x`, `y` (client pixels, origin top left), `expected` and `actual` as `[r, g, b]` (`actual` is `null` if no readback happened), `match` |
| `errors` | array of string | Every failure, in order. The first is usually the cause |
| `warnings` | array of string | Differences that are not failures of the probe, in order: a self-check that another vendor's driver does not meet |

### `adapter`

| Field | Probes | Source |
| --- | --- | --- |
| `name` | all | D3D9 `D3DADAPTER_IDENTIFIER9.Description`, DXGI `DXGI_ADAPTER_DESC1.Description`, `GL_RENDERER`, Vulkan `deviceName` |
| `vendor` | all | Vendor name from the PCI vendor ID; for OpenGL, `GL_VENDOR` |
| `vendor_id`, `device_id` | D3D, Vulkan | PCI IDs as `"0x1234"` |
| `subsys_id`, `dedicated_video_memory_mb`, `shared_system_memory_mb`, `dxgi_flag_software` | D3D11, D3D12 | `DXGI_ADAPTER_DESC1` |
| `driver` | D3D9 | `D3DADAPTER_IDENTIFIER9.Driver`, the D3D9 UMD file name |
| `driver_version` | D3D | D3D9 `DriverVersion`, DXGI `CheckInterfaceSupport(IDXGIDevice)`, as `a.b.c.d` |
| `driver_version_raw` | Vulkan | `VkPhysicalDeviceProperties.driverVersion` (encoding is vendor-specific) |
| `software` | all | True for a software renderer |
| `software_reason` | all | Why: `--warp requested`, `DXGI_ADAPTER_FLAG_SOFTWARE`, PCI vendor 0x1414 (Microsoft), a GDI generic pixel format, `VK_PHYSICAL_DEVICE_TYPE_CPU`, or a name containing Microsoft Basic Render/Display, WARP, GDI Generic, llvmpipe, lavapipe, softpipe or SwiftShader |

### `device`

| Probe | Fields |
| --- | --- |
| D3D9 | `create_function` (`Direct3DCreate9Ex` or `Direct3DCreate9`), `adapter_count`, `vertex_shader_version`, `pixel_shader_version`, `hw_transform_and_light`, `vertex_processing`, `device_type`, `coordinate_wrap_state_block` (false when the `WRAP` self-check recorded an error or a warning) |
| D3D11 | `driver_type` (`HARDWARE` or `WARP`), `feature_level`, `swap_effect` |
| D3D12 | `adapter_selection`, `max_feature_level`, `highest_shader_model`, `swap_effect` |
| OpenGL | `pixel_format`, `pfd_generic_format`, `pfd_generic_accelerated`, `context`, `gl_renderer`, `gl_vendor`, `gl_version`, `gl_shading_language_version`, `icd_loaded`, `icd_modules` (full paths of loaded modules that export `DrvSetPixelFormat`), `wgl_arb_create_context`, `swap_interval_1` |
| Vulkan | `loader` (path of `vulkan-1.dll`), `instance_version`, `physical_device_count`, `physical_devices` (one line per device), `device_type`, `device_api_version`, `driver_name`, `driver_info`, `driver_id` (Vulkan 1.2 devices), `swapchain_format`, `swapchain_images` |

A field is absent when the probe failed before it could learn it.

## Building

```sh
build/probes/build.sh          # all probes, x64 and x86, into out/probes/{x64,x86}/
build/probes/build.sh clean    # remove them
```

The build runs in the shared toolchain image (`build/toolchain/`: llvm-mingw
20261006 and glslang from the pinned Ubuntu snapshot). The only extra input is
Vulkan-Headers, pinned with a sha256 in `build/probes/PINS`. `build.sh` fetches
it into `out/probes/deps/` once and checks the hash. `make` rebuilds only what
changed. Binaries are stripped and linked without a timestamp, so a clean
rebuild gives the same bytes; this was checked on 2026-10-17 (hashes in
[docs/evidence/probes/README.md](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/probes/README.md)). After every build, `check-imports.sh` fails
the build if a probe imports a DLL that is not a Windows system DLL.

## Running in a guest

Use `tests/guest/Run-Probe.ps1`. It runs the probe in the logged-on user's
desktop session, where Vulkan works and the window is on the console, and it
records the modules the process loaded. `tests/guest/Set-TestDesktop.ps1`
creates an autologon user for that session. See `tests/guest/README.md`.

## The capset probe

`capset_probe.exe` is not a rendering probe and `Run-AllProbes.ps1` does not
find it. It sends the `VIOGPU_GET_CAPS` escape of the kernel driver with the
hostile arguments of card #751 (findings F1 and F3 of
[`docs/reviews/win3d-driver.md`](../../../docs/reviews/win3d-driver.md)).
A driver without `patches/kmd/kmd-check-capset-escape.patch` bugchecks the guest on
some cases, so run it in a disposable VM. Build it with:

```sh
build/probes/build.sh capset
```

The executables are `out/probes-extra/x64/capset_probe.exe` and
`out/probes-extra/x86/capset_probe.exe`.

```text
capset_probe.exe --case NAME [--out FILE]
```

It accepts only one PCI adapter with vendor `1af4` and device `1050`, reads
the supported capset ids with `D3DKMTQueryAdapterInfo`, and takes the lowest
one, k. Each run sends one escape:

| Case | Escape arguments | A fixed driver |
| --- | --- | --- |
| `read` | Capset k into a 65536-byte buffer of the process | Succeeds and writes the capset |
| `unmapped-address` | The address `0x10000`, which is not mapped | Fails |
| `read-only-address` | A read-only page of the process | Fails, the page is unchanged |
| `kernel-address` | 16 bytes to `0xFFFF800000000000` | Fails. Without the fix: bugcheck `0x50` |
| `bad-id` | Capset id `0xFFFFFFC0` + k | `STATUS_INVALID_PARAMETER_1`. Without the fix: an index far past the driver's array |
| `near-id` | Capset id 64 + k | `STATUS_INVALID_PARAMETER_1`. Without the fix: an index just past the array |

The program writes one JSON object to stdout and to `--out`. Before the
escape it flushes a record with `result` `requesting` to the file, so a
bugcheck leaves the request on disk. Afterwards `result` is `completed`,
`escape_status` is the NTSTATUS, `bytes_written` counts the bytes of the
process buffer that changed, and `pass` says whether the driver did what the
table says. Exit 0 is a pass, 2 a completed escape that did not pass, 1 an
API or adapter error, 3 a usage error.

`tests/kmd/capset-probe.sh <lab-dir> <vm> <out-dir>` runs every case in a VM
as a new standard user, and records a bugcheck of the guest
([`docs/acceptance/C751.md`](../docs/acceptance/C751.md)).

## The present probe

`present_probe.exe` is not a rendering probe either. It presents a blob
allocation with a Blt to a window of its own, in a Venus context of the
kernel driver: card #756, finding F8 of
[`docs/reviews/win3d-driver.md`](../../../docs/reviews/win3d-driver.md).
A driver without `patches/kmd/kmd-map-present-source-in-present.patch`
accepts the present of a blob that has no mappable flag and bugchecks the
guest with `0x119` a moment later, so run it in a disposable VM. Build it
with:

```sh
build/probes/build.sh present
```

The executables are `out/probes-extra/x64/present_probe.exe` and
`out/probes-extra/x86/present_probe.exe`.

```text
present_probe.exe --case NAME [--count N] [--out FILE]
```

It accepts only one PCI adapter with vendor `1af4` and device `1050`, makes
a device, a Venus host context (`VIOGPU_CTX_INIT` with capset 4) and a
context, and creates one allocation of 64 by 64 pixels with
`D3DKMTCreateAllocation`. The private data of the allocation names host
memory (`BlobMem` 2), a blob id and the blob flags. It then calls
`D3DKMTPresent` N times (default 1) with `Blt`, that allocation as the
source and its window as the destination.

| Case | Blob id | `BlobFlags` | A fixed driver |
| --- | --- | --- | --- |
| `unknown-blob` | One that the host does not know | 1 (mappable) | `D3DKMTCreateAllocation` fails: the driver creates the blob when the allocation is opened. The same without the fix |
| `mappable` | 0, shared memory of the Venus context | 1 | Every present is accepted and the guest stays up. The same without the fix |
| `not-mappable` | Device memory of the host's Vulkan driver | 0 | Every present fails. Without the fix: bugcheck `0x119` |
| `shareable` | The same | 2 (shareable) | Every present fails. Without the fix: bugcheck `0x119` |

For the last two cases the host must know the blob id, so the probe first
sends four Venus commands through the `VIOGPU_SUBMIT_CMD` escape:
`vkCreateInstance`, `vkEnumeratePhysicalDevices`, `vkCreateDevice` and
`vkAllocateMemory` of 64 KiB of memory type 0 with dma-buf export. It
writes the wire format itself and asks for no reply.

The window needs a surface, so the probe runs in the desktop session, not
in session 0. It writes one JSON object to stdout and to `--out`. Before the
first present it flushes a record with `result` `present_pending`, and after
the first accepted present one with `present_accepted`, so a bugcheck leaves
the state on disk. `pass` says whether the driver did what the table says.
Exit 0 is a pass, 2 another outcome that the guest survived, 1 an API or
adapter error, 3 a usage error.

`tests/kmd/present-probe.sh <lab-dir> <vm> <out-dir>` runs the four cases
in a VM as the standard user of the test desktop, through
`tests/guest/Run-Probe.ps1`, and records a bugcheck of the guest
([`docs/acceptance/C756.md`](../docs/acceptance/C756.md)).
