# Chromium's GPU process and the Direct3D 12 bridge

With the package installed, Chrome lost the GPU after a while: "It does not
appear your computer supports WebGL", `GL_VENDOR = Disabled`. Edge, Electron
applications and Windows' own WebView2 programs are the same kind of process
(GitHub #238). Mesa patch `patches/mesa/0095` and a second copy of
Microsoft's `d3d12.dll`, which the installer keeps, end that. This is a
workaround: the bridge still replaces `System32\d3d12.dll`
([known limits](known-limits.md#d3d12dll-is-replaced)).

## Cause

- A Chromium GPU process takes only Microsoft-signed DLLs once it is running
  (`Get-ProcessMitigation -Id <pid>`: `MicrosoftSignedOnly: ON`). The display
  driver's own DLLs are loaded before that, so they are accepted.
- When a page makes the browser look for GPU video encoders (anything with
  WebRTC or WebCodecs), the GPU process delay-loads `d3d12.dll` at that
  moment. The bridge is not signed by Microsoft. Windows refuses it with
  error 577 (invalid image hash), and the GPU process ends itself on purpose
  (`media::CreateD3D12Device`, `HandleDelayLoadFailureCommon`, exception
  0x80000003).
- After three such crashes the browser runs without a GPU until it is
  started again.

## What the package does

The bridge stays in System32 for every other program, and a Chromium GPU
process gets Microsoft's file:

- The installer keeps Microsoft's file as `vmlab-win3d\d3d12-native.dll`,
  for the bridge. It now also copies it to
  `System32\vmlab-win3d\native\d3d12.dll`, and the 32-bit file to the same
  place under SysWOW64 (`Update-NativeCopy` in `installer/vmlab-win3d.ps1`).
- `viogpu_d3d10.dll` loads that copy when it opens the adapter in a process
  whose command line has `--type=gpu-process` and that has no `d3d12.dll`
  yet. A Chromium GPU process opens the adapter before its restriction
  starts.
- The later load asks for `d3d12.dll` by its bare name, and the loader
  returns a module of that name that is already in the process before it
  looks in System32. That is why the copy keeps the name `d3d12.dll` and has
  a folder of its own.
- `D3D10UMD_YTTRIUM_NATIVE_D3D12_PRELOAD=0` in
  `C:\ProgramData\Yttrium\yttrium.ini` turns the preload off.

The GPU process then has what stock Windows gives it: Microsoft's Direct3D
12 runtime, which finds no Direct3D 12 driver for this adapter. WebGL runs
on Direct3D 11 as before.

### Windows servicing

A cumulative update can bring a new `d3d12.dll`. The `vmlab-win3d-repair`
startup task already keeps the new file as `d3d12-native.dll` before it puts
the bridge back. It now renews `native\d3d12.dll` from it in the same step,
and the install check (`Test-Installed`) requires the two to be the same
file. A browser that runs during the repair can have the old copy loaded, so
the task moves it aside and does not overwrite it. Uninstall removes the
folder file by file for the same reason.

## The check

`tests/e2e/chromium.sh <lab-dir> <vm> <evidence-dir>` runs in a lab that
`tests/e2e/run.sh --keep` left. It opens `tests/e2e/chromium/venc.html`, the
page of the GitHub issue: one WebGL context, a WebGPU adapter request, a
query for a hardware H.264 encoder, then a new WebGL context every 4 s for
20 s. `tests/guest/Test-ChromiumGpu.ps1` starts the browser in the user's
session with a new profile, and while the page runs it records the profile's
GPU processes and every `d3d12.dll` each one loaded.
The check restarts the VM at its start and before the control rows (see
Results for why).

| Row | What runs | Passes when |
| --- | --- | --- |
| `chrome` | Chrome for Testing (`tests/e2e/PINS`) with `--enable-features=D3D12VideoEncodeAccelerator,D3D12VideoDecoder` | WebGL is on the adapter before the query and in all five tries after it, the profile has no crash dump, one GPU process ran for the whole page, and every `d3d12.dll` it loaded is `vmlab-win3d\native\d3d12.dll` |
| `edge` | The guest's own Edge, with the same features | The same |
| `chrome-control`, `edge-control` | The same two with the preload off in `yttrium.ini` | The fault shows: a crash dump or a second GPU process. Without it the two rows above would prove nothing |
| `native` | The package's `d3d12_probe.exe` (x64) | It passes on the VirtIO adapter with `System32\d3d12.dll` and `vmlab-win3d\vkd3dcore.dll` loaded, and without the native copy: a native `D3D12CreateDevice` still gets the bridge |

Chrome for Testing has no field trials. The two features are what Chrome's
field trials turn on, and with them the GPU process loads `d3d12.dll` on this
page. A plain Chrome for Testing does not load it at all.

The check is a script of its own and is not part of `tests/e2e/run.sh`. The
adapter suite of card #148 has the same page as its rows `venc-chrome` and
`venc-edge`; when that suite is on `next`, the control rows and the module
record of this script belong in it.

## Results

Windows 11 (`x86_64/windows-11@26100.1742.11`) on vmlab-hv, 2026-11-20, with
the package built at `63ff2278`: the November base with the Mesa series 0001
to 0095. On each GPU `tests/e2e/run.sh --keep` ran first and passed 12 of 12,
then this check ran in the same lab. The kernel log had no NVIDIA Xid.

| Row | `AMD1` (Radeon 610M) | `Nvidia2` (RTX 5090) |
| --- | --- | --- |
| `chrome`, Chrome for Testing 158.0.8127.36 | pass: WebGL on the adapter in 5 of 5 tries, 0 dumps, 1 GPU process | pass: the same |
| `edge`, Edge 158.0.4370.33 | pass: 5 of 5, 0 dumps, 1 GPU process | pass: the same |
| `chrome-control` | the fault shows: WebGL in 0 of 5 tries, 3 dumps, 3 GPU processes | the same |
| `edge-control` | the fault shows: 0 of 5, 3 dumps, 4 GPU processes | the same |
| `native` | pass: the bridge and `vkd3dcore.dll` loaded, not the native copy | pass: the same |

In the `chrome` and `edge` rows the GPU process had `MicrosoftSignedOnly` on
and one `d3d12.dll` loaded, `C:\WINDOWS\system32\vmlab-win3d\native\d3d12.dll`.
In the control rows the half-second samples found no `d3d12.dll` in a GPU
process. WebGL's renderer
was `ANGLE (VirtIO, Yttrium VirtIO GPU 3D controller (0x00001050) Direct3D11
vs_5_0 ps_5_0, D3D11)` in every passing try. The page reported
`encoder supported: false` and no WebGPU adapter in all rows. An earlier run
on `Nvidia2` had Edge 157.0.4341.60 with the same result.

The installer was checked by hand in the same two labs, after the rows above:

- **Damage.** With `System32\vmlab-win3d\native\d3d12.dll` overwritten and
  the SysWOW64 folder deleted, `-Mode Repair` wrote both copies again.
- **Servicing.** With `d3d12.dll` in System32 and SysWOW64 replaced by a
  changed copy of Microsoft's file, as an update would, `-Mode Repair` kept
  the new file as `d3d12-native.dll`, renewed `native\d3d12.dll` to the same
  hash and put the bridge back. Putting the first file back the same way
  returned all three to their first hashes.
- **Uninstall.** `Uninstall.cmd` ran while Chrome for Testing was open and its
  GPU process had the native copy loaded. It ended with "removed; the machine
  matches its baseline", and both `vmlab-win3d` folders were gone after the
  restart.

One fault of the host showed in these runs and is not part of this fix:
vmlab-hv does not give back every file descriptor a guest GPU context used,
and it stops with "Too many open files" at 1024. After the probes of
`tests/e2e/run.sh` it held 825. That is why the check restarts the VM first
and again before the control rows.

## Limits

- A Chromium GPU process that loads `d3d12.dll` without ever opening this
  adapter gets no preload. `--use-angle=d3d11on12` is one. It ends up on
  Microsoft Basic Render Driver, as it would with Microsoft's file in
  System32. `--disable-gpu` and `--use-angle=swiftshader` load neither file.
- The preload keys on Chromium's command line. Another program with the
  same signature policy would need its own rule.
- WebGPU has no GPU adapter in the browser either way, and no hardware video
  encoder is offered.
- Between a Windows update that replaces `d3d12.dll` and the next boot, the
  copy is the older file. The GPU process then loads Microsoft's new file
  from System32 by itself, because the bridge is not there either.
- The real end is a native Direct3D 12 driver (Q4 of the real-adapter
  design): then no system file is replaced.
