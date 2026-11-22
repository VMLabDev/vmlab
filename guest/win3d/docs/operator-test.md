# Operator test of the win3d test build (card #409)

This page is for the person who tests the driver by hand, on helios and on
zeus. It says what was built, how to start a lab, what to try, what a good
result looks like, what is known not to work, and what to send back.

Nothing of this build is published: there is no release, no tag, no push to
a registry and no push to vmlab-templates.

## The build

| Item | Value |
| --- | --- |
| vmlab and vmlab-hv | `main` at commit 3161c859 (2026-11-22), built on helios |
| Driver package | version 0.1.3, guest commit `822f604b`, kernel driver `100.6.101.58116` |
| Package zip | `vmlab-win3d-0.1.3-test409-windows-x64.zip`, 25,721,092 bytes, sha256 `b887b84c5e1bd52efda16eb2ce12cafc7257cf0f72be758a32b73a641c510809` |
| Templates | `x86_64/windows-11@26100.1742.11#win3d` and `x86_64/windows-10@19045.2006.10#win3d`, rebuilt on this package in the store of the account vmlab on helios (the same directory as the operator's store) |
| Render server | the one of the vmlab-hv bundle, with the patches of the repository, sha256 `efd4f1a6…`. Each test VM ran it, not the host's `/usr/lib/virgl_render_server` |

The earlier variants (card #150) had driver `100.6.101.58111`. To see which
one a guest has, run in the lab directory:

```sh
vmlab exec win -- powershell -Command "(Get-Content C:\ProgramData\vmlab-win3d\bundle\manifest.json -Raw | ConvertFrom-Json).driver_version"
```

The version of the package is still 0.1.3, because `installer/VERSION` did
not change. Tell the two builds apart by the driver version or the commit.

## Where the files are on helios

Everything is under `/home/vmlab/src/stc/win3d-test/`, readable by the
account vmlab only.

| Path | What it is |
| --- | --- |
| `env.sh` | The environment of the test build: its `vmlab`, its vmlab-hv bundle, its own supervisor and state, and a host config that declares only `AMD1` and `Nvidia2` |
| `win3d-test/labs/windows-11/`, `win3d-test/labs/windows-10/` | One lab for each variant |
| `desktop.sh` | Prepares a logged-on desktop in a lab (below) |
| `report.sh` | Collects what a bug report needs (below) |
| `pkg/` | The package zip, its `SHA256SUMS` and release notes, and the pin change for vmlab-templates as a patch file |
| `tpl/` | A clone of vmlab-templates on the local branch `card-409` with the pin change committed (not pushed) |
| `zeus/` | The files to carry to zeus (below) |
| `src/` | The checkout of `main` that everything was built from. The probes are in `src/guest/win3d/out/probes/`, the adapter suite's programs in `src/guest/win3d/out/adapter/x64/` |
| `ev/` | The evidence of the automatic runs below |
| `old-variants/` | The variant disks of card #150, kept because stopped labs of earlier cards are clones of them |

The pin change points `fetch-deps.sh` of `windows-10` and `windows-11` at
the test zip. No release has that zip, so run `fetch-deps.sh` with
`WIN3D_ZIP=<path of the zip>`.

## Start a lab on helios

Use the second RTX 5090 (`Nvidia2`, `0000:05:00.0`, renderD129) or the
Radeon (`AMD1`, `0000:72:00.0`, renderD130). Never the display GPU
`0000:01:00.0`. The host config of `env.sh` does not declare the display
GPU, and a lab that names no GPU gets `AMD1`, so a lab started in this
environment cannot land on it.

```sh
sudo -iu vmlab
. ~/src/stc/win3d-test/env.sh
exec 9>~/src/stc/win3d-gpu-Nvidia2.lock; flock 9   # the agents' lock of that GPU, held until this shell ends
cd ~/src/stc/win3d-test/labs/windows-11            # or labs/windows-10
vmlab up                                           # first boot of a clone: 5 to 20 minutes
../../desktop.sh                                   # logs on the user win3d and restarts the guest once
vmlab console win --tcp                            # prints the VNC address for a viewer
```

- The lab files name `Nvidia2`. For the Radeon, change `host_gpu` to
  `"AMD1"` in `vmlab.wcl`, take `win3d-gpu-AMD1.lock`, and run `vmlab down`
  and `vmlab up`.
- `desktop.sh` copies the test scripts and probes to `C:\win3d`, activates
  the evaluation Windows, and makes the administrator `win3d` with the
  password `Win3d-probe-1!` and automatic logon. It prints
  `activation: licensed` when Windows is activated. A guest that is not
  activated shuts down about an hour after boot: then run `desktop.sh`
  again.
- `vmlab console win --tcp` runs as the account vmlab, which has no
  desktop. It forwards the console to a local TCP port. Connect a VNC viewer
  from your own session to the address it prints.
- `vmlab vm screenshot win shot.png` takes one console capture.
- End with `vmlab down`, or `vmlab destroy` to delete the clone. Then leave
  the shell, which frees the GPU lock.

If the kernel log shows an Xid (`journalctl -k | grep 'NVRM: Xid'`), stop
and do not try to bring the GPU back.

## What to try, and what good looks like

Run programs in the guest from the console, as the user `win3d`. The probes
are in `C:\win3d\probes\x64` and `x86`, the other test programs in
`C:\win3d\adapter`. To start a probe on the guest's desktop from the host:

```sh
vmlab exec win -- powershell -ExecutionPolicy Bypass -File 'C:\win3d\guest\Run-Probe.ps1' -Path 'C:\win3d\probes\x64\d3d11_probe.exe' -Seconds 30
```

These steps were run once as written, on 2026-11-22 in the windows-11 lab
on `Nvidia2`: `vmlab up` took 5 minutes, `desktop.sh` logged the user on,
the console address was `127.0.0.1:5900`, and the capture, the recording
(41 s of video) and `report.sh` worked.

| Code | Try | Good |
| --- | --- | --- |
| T1 | A desktop session: move and resize windows, open the Start menu, Settings and Explorer, drag a window fast for a minute | The desktop is drawn whole. No black window, no bands of an old frame that stay, no dark dashes. Device Manager shows "Yttrium VirtIO GPU 3D controller" without a warning sign. A "Test Mode" watermark in the corner is expected |
| T2 | `dxdiag`, Display tab | Direct3D and DirectDraw acceleration "Enabled", driver model WDDM 2.0 |
| T3 | Chrome and Edge with WebGL. Open `edge://gpu` or `chrome://gpu`, then a WebGL page such as `https://get.webgl.org/`. The test pages of the suite are `tests/e2e/adapter/pages/svb.html` and `venc.html` | The GPU page names "ANGLE (VirtIO, Yttrium VirtIO GPU 3D controller … Direct3D11". WebGL draws, and stays after several minutes and after a page that plays video. No "GPU process crashed" bar |
| T4 | Word, or another Office program (not in the template: install it) | The ribbon and its rounded buttons are filled without gaps, text is sharp, scrolling does not leave stale rows |
| T5 | A screen recording from the host: `vmlab record start win -o test.mp4`, use the desktop for a minute, `vmlab record stop win` | The video plays, shows the desktop and the pointer where they were, and has no black frames |
| T6 | Desktop Duplication in the guest: `C:\win3d\adapter\dupl_probe.exe`, or a program that captures the screen (the Snipping Tool's recording, OBS, a Teams screen share) | The capture shows the desktop, not black, and follows changes |
| T7 | The pointer: move it over text (I-beam), window edges (resize arrows) and a video that hides it | The pointer follows the mouse without lag and changes shape at once. The I-beam is black with a white outline (a known difference). It is at the same place in a screenshot and in the viewer |
| T8 | A Direct3D 9, 11 and 12 program: `d3d9_probe.exe`, `d3d11_probe.exe`, `d3d12_probe.exe` (also `opengl_probe.exe` and `vulkan_probe.exe`), each `--seconds 30`, x64 and x86. Then real programs of your own | Each window shows a turning pinwheel, the name of the API and the adapter line "Yttrium VirtIO GPU 3D controller" (OpenGL and Vulkan: "Virtio-GPU Venus (…)"), with a red, a blue and a green square in three corners. The frame rates of the automatic runs are in the table below |
| T9 | Restart the guest twice and repeat T1 | The same after each boot. No dwm crash, no black console |

## Results of the automatic checks

Run on 2026-11-22 on a new clone of each variant, on each GPU, with
`tests/e2e/run.sh --variant win3d` and then `tests/e2e/adapter.sh` in the
same lab. The kernel log had no Xid before or after any run.

| Variant | Host GPU | Done checks | Probe frame rates (D3D9, D3D11, D3D12, OpenGL, Vulkan; x64 / x86) | Adapter suite |
| --- | --- | --- | --- | --- |
| windows-11 `#win3d` | AMD1 | **12 of 12** | 59 / 57, 56 / 56, 53 / 51, 53 / 57, 58 / 58 | 23 pass, 4 fail, 0 not-run |
| windows-11 `#win3d` | Nvidia2 | **12 of 12** | 57 / 55, 55 / 55, 46 / 43, 54 / 55, 56 / 57 | 23 pass, 4 fail, 0 not-run |
| windows-10 `#win3d` | AMD1 | **12 of 12** | 57 / 58, 59 / 59, 44 / 56, 57 / 58, 58 / 58 | 23 pass, 4 fail, 0 not-run |
| windows-10 `#win3d` | Nvidia2 | **12 of 12** | 56 / 57, 58 / 58, 41 / 43, 56 / 58, 56 / 57 | 22 pass, 4 fail, 1 not-run |

The display runs at 60 Hz. Each probe drew on "Yttrium VirtIO GPU 3D
controller" or the Venus device, read its picture back correctly and showed
on the console. dxdiag reported hardware acceleration in all four runs.

**Render server.** In all four runs the VM's vmlab-hv was
`win3d-test/hv/libexec/vmlab-hv` and its render server was
`win3d-test/hv/libexec/virgl_render_server`, the patched one of the bundle
(sha256 `efd4f1a6…`, the `bundled-server-sha256` of the bundle's
`virglrenderer-BUILDINFO`). The stock server of the host is
`/usr/lib/virgl_render_server` (sha256 `35cee0b4…`) and did not run. On the
Nvidia2 labs `run.sh` wrote no `render-server.txt`: the lab name was too long
for the path it searches, so the record was taken from the running
processes (`ev/render-server/`).

The adapter suite ([acceptance.md](acceptance.md#adapter-suite)) asks
whether the device behaves like a real video card. A check that fails here
and in the baseline of `acceptance.md` is a known gap, not a new fault.

What the suite measured, the same in all four runs unless said:

| Check | Result | Measured |
| --- | --- | --- |
| `pointer`, `pointer-shapes` | pass | The pointer is not part of the frame, and its shapes, positions, rotation and hiding reach the host |
| `vblank-interval` | pass | Mean 16.662 to 16.667 ms on the 60 Hz mode |
| `present-rate` | pass | 60.0 to 60.2 frames per second with sync interval 1 |
| `flip-timestamps`, `scan-line` | pass | 60.00 Hz by the frame statistics, and the scan line moves through 768 lines |
| `tear-flip`, `tear-blt` | pass | 0 torn of about 1,260 console frames in each of the eight runs |
| `duplication`, `shared-texture` | pass | Desktop Duplication gives 8 lit frames, and a shared render target reads whole in all 4 cases |
| `d2d`, `d2d-tir`, `draws` | pass | Direct2D fills have no hole against WARP |
| `svb-chrome`, `svb-edge` | pass, but `svb-chrome` not-run on windows-10 Nvidia2 | WebGL draws `255,0,0,255` on the adapter in Chrome 158 and Edge 158. NOTRUN-TEXT |
| `venc-chrome`, `venc-edge`, `chromium` | pass | WebGL stays on the adapter in 5 of 5 tries, no crash dump, and the GPU process loads Microsoft's `d3d12.dll` |
| `wine-d3d10_1_d3d10_1`, `wine-d3d10core_d3d10core`, `wine-d3d10_device`, `wine-d3d10_effect`, `wine-d3d9_stateblock` | pass | 0 failures |
| `wine-d3d11_d3d11` | **fail** | 2 failures of 5,561,777 checks, the count of [acceptance/C374.md](acceptance/C374.md) |
| `wine-d3d9_d3d9ex` | **fail** | 7 failures of 2,387 on windows-11 (the count of [acceptance/C376.md](acceptance/C376.md)). On windows-10: 8 on AMD1, 2 on Nvidia2 |
| `wine-d3d9_device` | **fail** | The program ends on an access violation after its ATI1 texture checks, as in C376. Failed checks printed before it: 75 on windows-11, 99 and 62 on windows-10 |
| `wine-d3d9_visual` | **fail** | The same access violation, 1 failed check before it, as in C376 |

Windows 10 has no earlier Wine baseline: its counts above are the first.
The full tables are in `ev/<variant>-<gpu>/adapter/adapter.txt` under
`win3d-test/`.

## Known limits

The full list is in [known-limits.md](known-limits.md). The ones a tester
will meet:

- **L1, torn frames under heavy host load (bug #134).** About 1 console
  frame in 10,000 shows rows of two frames (2 of 20,000 measured on each
  GPU with the render server starved of CPU, [tearing.md](tearing.md)). It
  is gone with the next frame. Without host load the suite measures 0.
- **L2, the Direct3D 12 bridge is a workaround.** Direct3D 12 runs through
  vkd3d-proton behind a replaced `d3d12.dll`. A cumulative update or
  `sfc /scannow` puts Microsoft's file back until the next boot. Chrome and
  Edge load Microsoft's file by a second workaround
  ([chromium.md](chromium.md)). DRED is empty, and a program that ships its
  own `D3D12Core.dll` is untested.
- **L3, Test Mode.** The driver is not signed by Microsoft. The guest runs
  with Secure Boot off and test signing on, shows the "Test Mode" watermark,
  and trusts the test certificate of this build. Use it for lab VMs only.
- **L4, the first boot of a clone is slow.** Windows finishes its setup
  first: 5 to 20 minutes before `vmlab up` returns.
- **L5, the pointer.** A pointer larger than 64×64 is drawn by Windows into
  the frame, and the text I-beam is black with a white outline.
- **L6, Direct3D 9.** Wine's Direct3D 9 test programs still fail checks
  (the table above, and [acceptance/C376.md](acceptance/C376.md)): ATI1
  textures cannot be created, and a program that does not check for that
  can crash. Old games that use rare fixed-function states can draw wrongly.

## When something is wrong: what to send back

Run this in the lab directory while the fault shows, or right after it:

```sh
../../report.sh          # on zeus: ~/win3d-test/win3d-test-tools/report.sh
```

It writes `win3d-report-<time>.tar.gz` with:

- `vmlab-hv.log`, the host log of the VM, from
  `$XDG_STATE_HOME/vmlab/labs/<lab>/vms/win/` (the render server and the
  Vulkan layer write there too);
- `kernel-gpu.txt`, the NVIDIA, amdgpu and i915 lines of the kernel log;
- `console.png`, a console capture, `status-v.txt` (`vmlab status -v`: the
  hypervisor, the host GPU and its environment) and the lab file;
- from the guest: `install-journal.log` and `state.json`
  (`C:\ProgramData\vmlab-win3d\`), `gpu-state.json`
  (`tests/guest/Get-GpuState.ps1`: the device, its problem code, the driver
  files and their hashes), `dxdiag.txt`, and the display and crash events of
  the last six hours.

Send that file with: what you did, what you saw, which GPU and which
variant, and whether it happens again after `vmlab vm restart win`. For a
picture fault add a second `vmlab vm screenshot win <file>.png` that shows
it. For a program that fails, add its name, version and the API it uses.

## zeus

zeus has an Intel CPU and an NVIDIA laptop GPU. It is the one machine that
can show bug #136: images drawn through Direct3D 11 arrive with dark dashes
16 pixels long (64 bytes, one cache line), easiest to see in the sky of the
desktop wallpaper. No agent may change zeus, so the operator carries the
build there. This build has not run on zeus.

### The files

Copy the directory `/home/vmlab/src/stc/win3d-test/zeus/` of helios to
`~/win3d-test/` on zeus, and check it with `sha256sum -c SHA256SUMS`.

| File | Size in bytes | What it is |
| --- | --- | --- |
| `vmlab-win3d-0.1.3-test409-windows-x64.zip` | 25,721,092 | The driver package |
| `windows-11-26100.1742.11-win3d-test409.tar.zst` | 9,291,595,893 | `vmlab template export` of windows-11 26100.1742.11: the base, `#standard` and the new `#win3d` |
| `windows-10-19045.2006.10-win3d-test409.tar.zst` | 7,454,798,559 | The same for windows-10 19045.2006.10 |
| `vmlab-3161c859-linux-x86_64` | 45,054,192 | The `vmlab` binary of this build |
| `vmlab-hv-3161c859-linux-x86_64.tar.gz` | 6,824,673 | The vmlab-hv bundle of this build, with the patched render server |
| `win3d-test-tools.tar.gz` | 4,592,878 | `env.sh`, a host config example, a lab file, `desktop.sh`, `report.sh`, the probes and the guest test scripts |
| `0001-test-windows-pin-the-vmlab-win3d-0.1.3-test409-test-.patch` | 2,166 | The pin change for vmlab-templates |

### Set up

```sh
cd ~/win3d-test
mkdir -p bin hv
install -m 755 vmlab-3161c859-linux-x86_64 bin/vmlab
tar -xzf vmlab-hv-3161c859-linux-x86_64.tar.gz -C hv --strip-components=1
tar -xzf win3d-test-tools.tar.gz
. win3d-test-tools/env.sh
cp win3d-test-tools/config.wcl.example config/vmlab/config.wcl    # then put your home directory into vmlab_hv
vmlab template import windows-11-26100.1742.11-win3d-test409.tar.zst --trust-scripts
```

`env.sh` runs the test `vmlab` with its own supervisor, state and host
config under `~/win3d-test`, beside the installed vmlab. The template store
is the usual one. Use the binary of this build for the import: an older
vmlab may not read the sparse disk entries of the export.

If the store of zeus already has `windows-11@26100.1742.11`, the import
refuses. Then either add `--overwrite`, which replaces that version with
all its variants (destroy the labs that are clones of it first), or build
the variant from the zip:

```sh
git clone https://github.com/VMLabDev/vmlab-templates.git && cd vmlab-templates
git am ~/win3d-test/0001-test-windows-pin-the-vmlab-win3d-0.1.3-test409-test-.patch
cd windows-11 && WIN3D_ZIP=~/win3d-test/vmlab-win3d-0.1.3-test409-windows-x64.zip ./fetch-deps.sh && cd ..
vmlab template build windows-11@<the installed version> --variant win3d
```

The variant build boots a VM on the default GPU of the host config and
takes 10 to 25 minutes. Make `Intel1` the default for the build: the
build's check runs `vulkaninfo`, which hung on the RTX 4060 with an earlier
package ([zeus.md](zeus.md)).

### Start the lab and turn the host-memory rule on

```sh
. ~/win3d-test/win3d-test-tools/env.sh
cd ~/win3d-test/win3d-test-tools/lab
vmlab up
../desktop.sh
vmlab console win
```

The lab names `Nvidia1`, which on zeus is the RTX 4060 at `0000:01:00.0`.
(On helios that address is the display GPU. The lab file of zeus is for
zeus only.)

The host-memory rule of card #149 is the host GPU setting
`hide_uncached_memory`, off by default. To turn it on, add it to the NVIDIA
block of `~/win3d-test/config/vmlab/config.wcl`:

```wcl
gpu "Nvidia1" {
  address = "0000:01:00.0"
  default = true
  hide_uncached_memory = true
}
```

Then run `vmlab down` and `vmlab up`. Two things show that it is on:

- `vmlab status -v` has `VMLAB_VK_HIDE_UNCACHED=1` in the `env=` of the VM;
- `~/win3d-test/state/vmlab/labs/win3d-zeus-test/vms/win/vmlab-hv.log` has
  the line `vmlab-hide-uncached: memory type 2 (uncached) is hidden, guests
  use type 3 (cached)`.

### See whether the holes are gone

1. With the setting absent, boot the guest and wait for the desktop. Take
   `vmlab vm screenshot win off-1.png`. Restart (`vmlab vm restart win`)
   and take `off-2.png`.
2. Look at the smooth parts of the wallpaper (sky or sea) at 400 percent.
   A hole is a dark horizontal dash 16 pixels long. Count them. Card #136 counted 133 and 31
   in two boots on such a machine, with package 0.1.2.
3. Turn the setting on as above, and take `on-1.png` and `on-2.png` in two
   boots.
4. The bar of card #149 is 0 dashes in both boots with the setting on.

Also run T1, T3 and T8 of the list above with the setting on. Send back
the four pictures, the dash counts, and `report.sh`'s file of an "off" boot
and of an "on" boot. Two answers are open until this is measured: whether
the rule ends #136, and whether it should be on for every NVIDIA host or
only where the guest driver's own probe fails.

The laptop GPU powers down when idle. If it falls off the bus (Xid 79 in
`journalctl -k`), stop: it stays unusable until zeus restarts
([zeus.md](zeus.md)).
