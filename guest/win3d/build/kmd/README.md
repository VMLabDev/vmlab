# kmd

Builds the Yttrium viogpu3d kernel-mode display driver (KMD), its two
user-mode helpers, and the INF files, for x64 Windows 10 and 11. The source is
`arehnman/yttrium-virtio-gpu` at tag `yttrium-experimental-2026-11-02`
(`e9917ae25b2597306d483e1063dba08658139bc9`), a fork of virtio-win's
kvm-guest-drivers-windows, plus the patches in `patches/kmd/`.

Upstream builds with Visual Studio and the WDK on Windows. This build runs on
Linux: clang-cl and lld-link from llvm-mingw compile and link against the
headers and kernel libraries in Microsoft's WDK and SDK NuGet packages. No
Windows tool runs at any step.

The build does not sign anything. `build/signing/sign-driver.sh` signs the
output, and `build/package/build.sh` assembles the package
([decision 0001](../../docs/decisions/0001-signing.md)).

## Run

```sh
build/kmd/build.sh
```

The script downloads the pinned inputs on the host, checks each against its
sha256 in `PINS`, and caches them in `$WIN3D_CACHE` (default `.scratch/cache`
in the repository). It then builds the image `vmlab-win3d/kmd-build:1` from
`Dockerfile` and runs `inner.sh` in it as your uid, with `--network none`. The
sources, kits and toolchain are mounted read-only. Each run deletes
`out/kmd/` first.

| Variable | Default | Effect |
| --- | --- | --- |
| `WIN3D_CACHE` | `.scratch/cache` | Download and unpack cache |
| `KMD_DRIVER_VERSION` | `100.6.101.58120` | Overrides the PE version resource and INF `DriverVer`. The default build component is 58101 plus the number of KMD patches. `58120` reads a message of the helpers' pipe inside its buffer and returns an error when `DxgkCreateAllocation` has no memory (the two patches of card #757). `58118` has the checks of the capset escape and the capset size of the host (`kmd-check-capset-escape.patch`, card #751) together with the size check of the submit escape (`kmd-check-submit-escape-size.patch`, card #752). The build of each of those two cards alone was `58117`. A release that rebases or removes patches can set a higher version for Windows driver ranking. This version takes the vblank from vmlab-hv's frame clock when it offers the feature bit (`0012`). `58112` made a lost host GPU a removed device when vmlab-hv reports it (`0011`). `58111` had vmlab-hv copy a Blt present when it offers the feature bit (`0010`). `58110` sent the host-ordered flip when vmlab-hv offers it (`0009`). `58109` reported a flip with a vsync only after it was sent to the host (`0008`). `58108` was an experiment of card #143 that is not in this repository: do not reuse the number. `58107` added the present copy into a mappable blob (`0006`). `58106` added the hardware pointer (`0005`). `58105` was the package rebased to Yttrium's November 2026 release. `58104` was the last on the September release, with native x86 UMD mapping-fault handling. Its KMD recovery logic was unchanged from `58103`, which retains drained-fence diagnostics above initial recovery `58102` and GDI context creation `58101`. |

## Trace build

```sh
KMD_TRACE=1 KMD_DRIVER_VERSION=100.6.101.58190 build/kmd/build.sh
```

`KMD_TRACE=1` compiles the driver with `VIOGPU_FLIP_TRACE=1`
(`patches/kmd/kmd-flip-trace-build.patch`). The driver then keeps a ring of 65536 timed records,
and a DRIVERPRIVATE escape (magic `0x56475446`) returns the records after a
sequence number, 1024 at a time. `tests/tearing/Get-FlipTrace.ps1` reads it
in the guest into a CSV file. Give a trace build its own driver version, so
that a guest shows which driver it runs. A trace build is for a lab: do not
release one. `BUILDINFO` says `trace_build 1`.

A record is `seq, time, event, context, flags, fence, response, pending`.
The time is `KeQueryInterruptTimePrecise`, in units of 100 ns.

| Event | Meaning | Fields |
| --- | --- | --- |
| 1 | A DMA buffer was submitted | context, fence = the submission fence of Windows |
| 2 | A DMA buffer was reported complete to Windows | context, fence |
| 3 | Preemption request | flags, pending = the preemption fence |
| 5 | The host answered a control queue command | context, flags = the request's flags (1 fence, 2 ring index), fence = the virtio fence, response = the response type |
| 6 to 9 | Timeout, reset, restart, power | as in `docs/tdr-recovery.md` |
| 10 | vmlab-hv reported the host GPU lost (`0011`), once a boot | none |
| 16, 22 | The copy of a Blt present begins, ends (with `0010` on vmlab-hv: the host command is queued) | context, flags = source resource, fence, response = destination resource |
| 17 | A flip from a DMA buffer: `SET_SCANOUT` and `RESOURCE_FLUSH` are sent | context, flags = resource, fence |
| 18 | `SetVidPnSourceAddress` | flags = resource, fence = the id of the process that made it |
| 19 | The vsync timer, or with `0011` on vmlab-hv the completion of a host vblank, sends the recorded flip | flags = resource |
| 20 | One packet of a DMA buffer goes to the control queue | context, flags = ring index, fence, response = command type (0 NOP, 1 SUBMIT, a timeline submit has a ring index) |
| 21 | A host context was created | context, flags = capset, fence = process id |
| 23 | The interrupt routine reported a vsync (`0008`) | flags = the reported address divided by 4096, response = 1 when the address came from the queue of flips |
| 24 | A queued flip was not sent to the host yet when a vsync was reported (`0008`) | flags = the flips in the queue, response = 0 the address was held back, 1 it was reported (knob 2) |
| 25 | The host answered a vblank wait (`0012`, interrupt routine) | flags = the host's tick (low 32 bits), fence = the host time of the tick in ns, response = 1 when this answer raised the vsync, 0 when the completion of the wait raises it after it sent the waiting flip |
| 26 | The `VIOGPU_RES_SET_SCANOUT_BLOB` escape was decided | context = the id of the calling process, flags = resource, fence = the id of the process that has the scanout, response = 0 served, 1 no such scanout, 2 the caller did not make the allocation, 3 the caller does not have the scanout, pending = the id of the process that made the allocation |

Events 1 to 10 are the records of the 256-record ring of `0004` and `0011`, without the
bare interrupt (event 4). A release build has that small ring only.

A second escape (magic `0x56475453`, then a knob and a value) changes the
trace build for an experiment. `Get-FlipTrace.ps1` sets both knobs:

| Knob | `Get-FlipTrace.ps1` | Effect |
| --- | --- | --- |
| 1 | `-Stall <microseconds>` | Holds every present copy for that time when it is half done, at most 20000. 0 turns it off |
| 2 | `-ReportUnsent 1` | A vsync reports queued flips in the order of the driver before `0008`, also a flip that was not sent yet. 0 is the patched order |

Both are off after a restart of the guest.

## Outputs

```
out/kmd/x64/viogpu3d.sys                  the KMD
out/kmd/x64/viogpu3d.pdb                  its symbols (not packaged)
out/kmd/x64/vgpusrv.exe                   the "Yttrium VirtIO Resolution Service"
out/kmd/x64/viogpuap.exe                  started by vgpusrv in each user session
out/kmd/x64/viogpu3d.inf                  kernel-only INF
out/kmd/viogpu3d_x64_wow64.inf.template   full INF, the input to build/package/make-inf.py
out/kmd/BUILDINFO                         driver version, pins, patch hashes, output sha256
```

## How it builds

`inner.sh` runs these steps in the container:

1. Unpacks llvm-mingw and the two NuGet packages (they are zip files).
2. Copies `VirtIO/`, `viogpu/` and `build/` from the source tree to the fixed
   path `/tmp/kmd-work/src`, and applies the patches in `patches/kmd/series` order.
3. Sets `SOURCE_DATE_EPOCH` and the INF date to upstream's tag date,
   2026-11-02.
4. Runs `compile-kmd.sh`, `compile-helpers.sh` and `stampinf.py`.

### The KMD (`compile-kmd.sh`)

The compiler flags mirror the WDK's `WindowsKernelModeDriver10.0` toolset
for "Win11 Release|x64": `/kernel /GS /Gz /Zp8 /GR- /Zc:wchar_t- /EHs-c- /O2`,
`NTDDI_VERSION=0x0A000010`, the 10.0.26100.0 kit. `/Brepro` keeps the
output deterministic.

- **Case.** Windows sources and kit headers `#include` names in any case.
  `mkoverlay.py` writes a clang VFS overlay that makes the kit include
  directories and the source tree case-insensitive.
- **Intrinsics.** clang's own `intrin.h` is force-included before `wdm.h`, so
  the port I/O intrinsics are defined inline rather than as bare MSVC
  builtins.
- **WPP.** `tracewpp.exe` exists only on Windows. The script writes empty
  `.tmh` stubs and defines `WPP_INIT_TRACING` and `WPP_CLEANUP` as no-ops, so
  the driver has no ETW provider.
- **Sources.** VirtioLib (`VirtIO/VirtIOPCICommon.c`, `VirtIOPCILegacy.c`,
  `VirtIOPCIModern.c`, `VirtIORing.c`, `VirtIORing-Packed.c`) is archived with
  `llvm-lib`. The driver is 14 `.cpp` files from `viogpu/common/` and
  `viogpu/viogpu3d/`, listed in the script.
- **Resources.** `viogpu3d.rc` is UTF-16 and includes mixed-case headers. The
  script converts it to UTF-8, preprocesses it with clang (which has the
  overlay), and compiles the result with `llvm-rc /no-preprocess`.
- **Link.** `lld-link /DRIVER /SUBSYSTEM:NATIVE,10.00 /ENTRY:GsDriverEntry
  /NODEFAULTLIB` against `bufferoverflowfastfailk.lib`, `ntoskrnl.lib`,
  `hal.lib`, `wmilib.lib` and `displib.lib` from the WDK.

### The helpers (`compile-helpers.sh`)

`vgpusrv.exe` (upstream's `viogpusc` project) and `viogpuap.exe` use only
Win32, cfgmgr32, D3DKMT and WTS APIs. They build with llvm-mingw's
`x86_64-w64-mingw32-clang++`, linked statically like upstream's `/MT` build.
mingw-w64 has no `d3dkmthk.h`, so the script takes it, `d3dukmdt.h` and
`d3dkmdt.h` from the SDK package and searches them after the mingw headers.
`mingw-compat.h` is force-included to supply `__FUNCTIONW__` and two legacy
SAL macros.

### The INF (`stampinf.py`)

The source has an INF template, `viogpu/viogpu3d/viogpu3d_x64_wow64.inx`.
Upstream's MSBuild turns it into an INF with three Windows-only steps.
`stampinf.py` does the same:

| Placeholder or entry | Becomes | Upstream step |
| --- | --- | --- |
| `$ARCH$` | `amd64.10.0`, so the decoration is `NTamd64.10.0...16299` | `stampinf.exe -a`, with the `InfArch` from `viogpu3d.vcxproj` |
| `DriverVer` | `11/02/2026,<KMD_DRIVER_VERSION>` | `stampinf.exe` |
| `INX_COPYRIGHT_1`, `INX_COPYRIGHT_2`, `INX_COMPANY`, `INX_PREFIX_VENDOR`, `INX_PREFIX_VIRTIO`, `INX_PREFIX_QEMU`, `INX_PLATFORM_DRIVERS_DIR`, `_INX_SUBSYS_VENDOR_ID` | The Hardsoft vendor strings, `13` (driver store), `1AF4` | `Driver.Common.targets` |
| Lavapipe files (`vulkan_lvp_*.dll`, `lvp_icd.*.json`), the `yttrium12` D3D12 slot, and the driver-store section `VioGpu3D_Files.Yttrium12` with `yttrium12.dll` and `d3d11on12.dll` | Removed, because those payloads are not built (below) | `tools/prepare_package_inx.ps1` |

The November release moved `yttrium12.dll` out of System32 into the driver
store (`%13%\x64\yttrium12.dll`) and added a patched `d3d11on12.dll` beside
it. Neither upstream repository has the source of either file at the pinned
commits, and upstream's own build only packages them when
`YTTRIUM12_DLL_x64` and `YTTRIUM12_ON12_DLL_x64` point at prebuilt binaries.
This package ships nothing it cannot build from source, so both stay out,
which is what upstream's script does when those variables are empty.

It writes two files:

- `viogpu3d_x64_wow64.inf.template`: the full INF. It still lists every Mesa
  file upstream ships, including `libEGL`, `libGLESv*` and `opengl32`.
  `build/package/make-inf.py` turns it into the package INF: it drops those
  files and the machine-wide `TdrDebugMode` and `EnablePreemption` values,
  and sets `UserModeDriverName[Wow]` to `viogpu_d3d10.dll` three times. It
  fails if the template no longer has a line it expects to change.
- `x64/viogpu3d.inf`: the kernel-only INF. It installs `viogpu3d.sys`,
  `vgpusrv.exe` and `viogpuap.exe` with no UMD, OpenGL or Vulkan entries.
  The signing load test uses it.

The INF matches `PCI\VEN_1AF4&DEV_1050&SUBSYS_11001AF4&REV_01`. It installs
`viogpu3d.sys`, `vgpusrv.exe` and `viogpuap.exe` to DIRID 13 (the driver
store), adds the kernel service `VioGpu3D` and the auto-start service
`vgpusrv`, and writes `MSISupported` = 0 and `MessageNumberLimit` = 4 under
the device's interrupt-management key.

## Pins

`PINS` lists each input with its sha256 (git sources by commit). The image's
Debian packages are pinned by version in `Dockerfile`: `python3`, `xz-utils`,
`patch` and `unzip`.

| Input | Version |
| --- | --- |
| yttrium-virtio-gpu | `e9917ae2`, tag `yttrium-experimental-2026-11-02` |
| llvm-mingw | 20261006 (LLVM 23.1.3, ucrt) |
| Microsoft.Windows.WDK.x64 | 10.0.26100.6584 |
| Microsoft.Windows.SDK.CPP | 10.0.26100.6584 (headers only) |
| debian | 12, by digest |

`docs/stack.md` has the full table, generated from `PINS`.

## Patches

| Patch | What it does |
| --- | --- |
| `kmd-r10-copy.patch` | Converts R10G10B10A2 sources to 8-bit RGBA or BGRA in the KMD's CPU present blit, so 10-bit back buffers present correctly. It is a byte-identical copy of the prior art's `R/kmd-r10-copy.patch` on the same base (prior-art §2.2, `NW` F93). |
| `kmd-cache-gdi-shadow-and-staging-surfaces.patch` | Sets `Flags.Cached` on the GDI shadow and staging surfaces, the destination of every Blt present, so the KMD's CPU present copy does not write uncached memory. It took D3D9 windowed presents from 25–31 fps to the refresh rate on AMD hosts ([`docs/present.md`](../../docs/present.md)). |

| `kmd-create-context-for-gdi-devices.patch` | Creates the legacy host context when the kernel device is constructed, so GDI devices that present without a UMD have one ([`docs/gdi-context.md`](../../docs/gdi-context.md)). |
| `kmd-recover-from-timeouts.patch` | Timeout (TDR) and D3/D0 recovery: quiesces the transport, cancels old requests, keeps OS allocations and restores legacy host state at restart ([`docs/tdr-recovery.md`](../../docs/tdr-recovery.md)). |
| `kmd-hardware-pointer.patch` | Hardware pointer: reports a 64×64 pointer when `HWCursor` is set, sends Windows' shape, position and visibility on the cursor queue, restores them after a device reset, and sets `HWCursor` to 1 in the INF ([`docs/pointer.md`](../../docs/pointer.md)). |
| `kmd-present-blt-into-mappable-blob.patch` | Desktop Duplication: a present copy of the Blt kind may write into a blob when source and destination are both mappable blobs. The destination is mapped in `Present` and written at the image's offset in the blob ([`docs/present.md`](../../docs/present.md#desktop-duplication)). |
| `kmd-flip-trace-build.patch` | The [trace build](#trace-build): a 65536-record ring and its escape, compiled only with `VIOGPU_FLIP_TRACE=1`. A release build is unchanged by it. |
| `kmd-report-flips-after-they-are-sent.patch` | A vsync reports the address of a flip only after the vsync timer sent that flip to the host. Before, a flip that Windows recorded between the timer and the interrupt was reported one tick early, and Windows gave the buffer on screen back to the compositor 16 ms early. It is not the cause of the torn frames of #134 ([`docs/tearing.md`](../../docs/tearing.md)). |
| `kmd-host-ordered-flip.patch` | Acks vmlab-hv's private feature bit `VIRTIO_GPU_F_VMLAB_ORDERED_FLIP` (23) when the host offers it, and then sends every flush of the scanout as `VIRTIO_GPU_CMD_VMLAB_ORDERED_FLIP` (`0x0f00`), which the host answers only once the frame is on its display, after every fence of its renderer and the implicit fence of the scanout buffer have signalled. Without the bit (QEMU) the driver sends `RESOURCE_FLUSH` as before ([`docs/flip.md`](../../docs/flip.md)). |
| `kmd-host-present-copy.patch` | With the same bit, sends the copy of a Blt present as `VIRTIO_GPU_CMD_VMLAB_PRESENT_COPY` (`0x0f01`) ahead of the present's fenced NOP, in place of the copy on a guest CPU. The boxes are clipped by the code of the CPU copy. Without the bit, or when the command cannot be queued, the CPU copy runs as before ([`docs/present-copy.md`](../../docs/present-copy.md)). |
| `kmd-host-gpu-lost.patch` | With the same bit, reads vmlab-hv's private event `VIRTIO_GPU_EVENT_VMLAB_DEVICE_LOST` (bit 31 of `events_read`) on the configuration interrupt and at each negotiation. The adapter is then lost for the boot: every 3D device is context-lost and its UMD gets `STATUS_DEVICE_REMOVED`, a new 3D context is refused, a DMA buffer of a GDI device retires without the host, and a restart after a timeout restores no host objects ([`docs/device-lost.md`](../../docs/device-lost.md)). |
| `kmd-host-vblank.patch` | With the same bit, starts no vsync timer: it keeps one `VIRTIO_GPU_CMD_VMLAB_VBLANK` (`0x0f02`) wait on the control queue, which vmlab-hv answers at each tick of its 60 Hz frame clock with the tick and its host time. The answer raises the vsync, or, when a flip is waiting, the completion of the wait does, right after it sent the flip. No empty command is sent per tick. Without the bit, or when the host refuses the wait, the timer and its NOP run as before ([`docs/vblank.md`](../../docs/vblank.md)). |
| `kmd-check-capset-escape.patch` | Card #751, findings F1, F3 and F6 of [`docs/reviews/win3d-driver.md`](../../../../docs/reviews/win3d-driver.md). The `VIOGPU_GET_CAPS` escape probes the user address before it copies the capset there, and refuses a capset id outside 1 to 63. A capset whose host size is above 1 MiB is not supported, the response size is computed without a wrap, and the response buffer is zeroed. The bounds are in `viogpu/common/viogpu_capset_bounds.h`, which `tests/kmd/capset_bounds_test.cpp` compiles on the build host ([`docs/acceptance/C751.md`](../../docs/acceptance/C751.md)). |
| `kmd-check-submit-escape-size.patch` | Checks the command size of the `VIOGPU_SUBMIT_CMD` escape with a subtraction. The sum the driver made before wrapped in 32 bits for a size of `0xFFFFFFF4` or more, and the driver then copied that many bytes out of the escape buffer. Such a request now fails with `STATUS_INVALID_BUFFER_SIZE` ([`docs/acceptance/C752.md`](../../docs/acceptance/C752.md)). |
| `kmd-read-helper-pipe-message-inside-buffer.patch` | Card #757, finding F9 of the same review. The two helpers read a message of their pipe through one function, `VioGpuPipeReadMessage` in `viogpu/shared/pipe_bounds.h`, that counts in bytes, keeps the buffer a terminated string and drops a message that does not fit. Before, `PipeServer::Read` asked for 4096 bytes into a buffer of 2048, and both loops put the second piece of a message at twice its offset. `tests/kmd/pipe_bounds_test.cpp` compiles the header on the build host ([`docs/acceptance/C757.md`](../../docs/acceptance/C757.md)). |
| `kmd-check-create-allocation-memory.patch` | Card #757, finding F10. `DxgkCreateAllocation` returns `STATUS_INSUFFICIENT_RESOURCES` when the pool has no memory for the allocation object, and `STATUS_INVALID_PARAMETER` for a `NumAllocations` other than 1. The pool `operator new` of the driver is `noexcept`: without it clang ran the constructor on the NULL pointer before any check of the caller, at every `new` of the driver ([`docs/acceptance/C757.md`](../../docs/acceptance/C757.md)). |
| `kmd-scanout-only-for-its-owner.patch` | Closes the two ways a program had to the scanout without Windows. `DxgkDdiRender` refuses `VIOGPU_CMD_PRESENT_FLIP` in the command stream of a program with `STATUS_PRIVILEGED_INSTRUCTION`: only `Present` writes that command. The `VIOGPU_RES_SET_SCANOUT_BLOB` escape is served only for the process that made the allocation Windows last set as the source of the scanout, for an allocation that this process made, on scanout 0. Every other caller gets `STATUS_ACCESS_DENIED` ([`docs/acceptance/C755.md`](../../docs/acceptance/C755.md)). |

0001 has no mail header; 0002 has a short one, which `patch` skips. `PINS`
records the sha256 and origin of 0001 and 0002.

0005 is the patch of GitHub #235, ported over 0004 and changed as
[`docs/pointer.md`](../../docs/pointer.md) says.

0001 and 0002 apply to `e9917ae2` unchanged. 0003 and 0004 were rebased:
upstream's November release added the native GPU clock, per-process
IO-space views for blob mappings (`viogpu_blob_user_view.cpp`) and an unwind
of failed adapter starts in the code they touch. In 0004 the reset path now
revokes user blob mappings with `VioGpuUnmapBlobUserView` in place of
`DxgkCbUnmapMemory`, takes the blob-map `KMUTEX` that replaced the fast
mutex, and resets the native clock cache with the host objects.

## Reproducibility

The build was bit-reproducible on this host at the September base
(`b99d951a`): a run on 2026-10-17 gave the same sha256 for all six outputs,
including the PDB, as the run recorded in [docs/evidence/kmd/00-signing-host.txt](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/kmd/00-signing-host.txt).
At `e9917ae2` with patches 0001 to 0004 and version `100.6.101.58105`, two runs
on 2026-11-19 (one of `build/kmd/build.sh` alone, one inside
`scripts/build.sh`) gave the same sha256 for all six outputs:

```
ff46043d31193414a86051aa049aa2ec5cad15a8c5e35cac146ae1041c9ebfd6  x64/viogpu3d.sys
deff13da3ec96ce25f314e04eac18aeb27c37b30e3e35cfe27e56ef7d55ccb3e  x64/viogpu3d.pdb
b5f16e3e38b953aad760fadd8fcea6db94b77c3d62a0b4e614a263d38a56a37a  x64/vgpusrv.exe
9e783bc6f827dc3edb199132adf26ed8b0e99c6b9f61e3e451cecf22555a9df9  x64/viogpuap.exe
690f9b47009f93e1b3c9225a16521216abe2412aef3d4acd3b529f3b9915aaab  x64/viogpu3d.inf
15404c1100bb30ddc0a8abcff1b773e8fb02d736cc412f32f8c7b6b22c25c56d  viogpu3d_x64_wow64.inf.template
```

The work tree is always `/tmp/kmd-work` in the container, `/Brepro` removes
timestamps from objects and the PE, and the helpers use
`-ffile-prefix-map` and `--no-insert-timestamp`.

## Caveats

- **Release, not Debug.** The build is upstream's Release configuration
  with WPP stubbed, so the driver has no ETW provider, and its `DbgPrint`
  output needs a kernel debugger. The prior art ran upstream's Debug build,
  which prints to the kernel debugger and breaks into it when one is
  attached. This script has no such variant. What it has is the
  [trace build](#trace-build), which needs no debugger.
- **lld-link `/SECTION` replaces attributes.** `INIT,d` alone left
  `DriverEntry` in a non-executable section, and the guest bugchecked 0x1A on
  load. The link uses `/SECTION:INIT,ERD`. See
  [docs/evidence/kmd/README.md](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/kmd/README.md), "Earlier runs".
- **INF decoration.** `NTamd64...16299`, without `.10.0`, puts 16299 in the
  ProductType field, and the INF then matches no device. `stampinf.py`
  writes `amd64.10.0`, as upstream does.
- **Code 43 with the kernel-only INF.** Installed alone, the KMD loads and
  starts, then reports Code 43, because there is no UMD. The full package
  reports problem code 0 ([docs/evidence/package/README.md](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/package/README.md)).
- **Needs a 3D device.** The KMD requires `VIRTIO_GPU_F_VIRGL`, so it fails
  to start on a plain `virtio-vga`. Use vmlab's `gpu { mode = "vulkan" }`.
- **No static analysis.** Upstream's tree has Static Driver Verifier scripts
  (`RunSdv.bat`), which need Windows. This build runs none. clang runs with
  `-Wno-error=incompatible-function-pointer-types` and several
  MSVC-compatibility warnings off.
- **x64 only.** The script builds no other architecture.
