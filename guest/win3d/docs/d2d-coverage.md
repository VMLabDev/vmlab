# Direct2D coverage: forced-sample-count draws without the interlock

Direct2D on feature level 11.1 antialiases filled shapes with
target-independent rasterization. It draws triangle fans with
`ForcedSampleCount` 16 into a single-sampled `R16_UINT` texture, the pixel
shader writes `SV_Coverage`, and the output merger XORs it into the texture.
A second pass turns the 16 coverage bits of each pixel into an alpha.

## The fault

Upstream's driver draws the coverage pass once, at 16 samples with no
attachment, and does the XOR in the fragment shader under a pixel interlock
(`VK_EXT_fragment_shader_interlock`; `yttrium_pipeline.c`,
`yttrium_shader.c`). Where the lock does not hold between two fragments of
one pixel whose coverage is disjoint, one of the two XORs is lost. That is
every pixel on an edge that two triangles of a fan share. Microsoft Word's
ribbon showed short pale gaps in its rounded boxes, in other places at every
start (GitHub #236, found on a laptop RTX 4050 with package 0.1.2-alpha).

## The fix

`patches/mesa/0094` sends such a draw as one single-sampled draw per sample,
with the texture as an ordinary colour attachment and the blender's logic op
XOR. No lock is needed, because the blender works in primitive order.

- For sample *i* the viewport is moved so that the pixel centre falls on that
  sample's position (D3D11's standard pattern).
- One pipeline serves all the draws: each draw's viewport has its depth range
  collapsed to (*i* + 0.5) / 16, and the fragment shader takes its
  `SV_Coverage` bit from the fragment depth.
- It is taken only for what Direct2D does here: one `R16_UINT`
  single-sampled target, logic op XOR, no blending, no depth buffer, and a
  shader that reads the sample mask and does no per-sample shading.
  Everything else goes the old way.

`FORCED_SAMPLE_PASSES=0` in `C:\ProgramData\Yttrium\yttrium.ini` (or
`D3D10UMD_YTTRIUM_FORCED_SAMPLE_PASSES=0` in the environment) brings the
interlock path back. The setting is read once per process.

Limits:

- 16 draws stand in for one. The cost is not measured.
- Indirect draws still take the interlock path.
- Checked on `windows-11` on `AMD1` and `Nvidia2` only.

## The check

`tir_probe.exe` (source `probes/d3d11-tir/tir_probe.cpp`, built by
`probes/d3d11-tir/build.sh`) draws what Direct2D draws, with Direct3D
11.1 itself: 96 discs and 32 stars as triangle fans, `ForcedSampleCount` 16,
an `R16_UINT` target, a pixel shader that returns `SV_Coverage`, and the
logic op XOR. It draws the scene three times on new devices and once on WARP
and compares the 16 coverage bits of every pixel. Its header says what it
reports.

`tests/e2e/tir.sh <lab-dir> <vm> <evidence-dir>` runs the x64 and the x86
probe in a lab kept with `tests/e2e/run.sh --keep`. For each pipeline the
driver writes one line into `C:\ProgramData\Yttrium\yttrium-errors.log`:
`forced-sample draw <process> ... coverage=passes` on the new path, and
`requested=16 hw=<n> ... coverage=native` or `coverage=expanded` on the
interlock path. The script passes when no pixel differs from WARP or between
runs and every line about the probe says `coverage=passes`. An x64 probe
that draws the right picture without such a line did not reach the path, and
that fails. The x86 driver wrote no such line in any run, and why is not
known, so the x86 probe is judged by its picture alone. On `AMD1` and
`Nvidia2` that is enough, because the interlock path gives a wrong picture on
both (the table below).

As the driver makes that log, only the process that made it can append to
it, and that is `dwm.exe` most often: other users get read access. A line of
a program that runs as the logged-on user is then lost. The script first
gives every user the right to append.

The adapter suite (`tests/e2e/adapter.sh`, which `run.sh --apps` ends with)
runs it as its check `d2d-tir`
([acceptance.md](acceptance.md#adapter-suite)).

A probe that fills and strokes shapes through Direct2D itself was not enough.
One written for this patch (filled and stroked rounded rectangles, ellipses,
stars and Bezier shapes, and a figure of 2000 segments) never reached the
path: with the log removed before each run, the driver made no log at all,
on either path. The `d2d_probe.exe` of the adapter suite fills a subset of
those shapes. Which shapes make Direct2D choose target-independent
rasterization was not found. `dwm.exe`,
`copilotapphost.exe` and Word's Current Channel build reach it. Word 2024
LTSC (16.0.17932) does not.

## Results (2026-11-20, helios)

One `windows-11` lab (`26100.1742.11`, driver `100.6.101.58106` with 0094),
first on `AMD1` (Radeon 610M, RADV), then the same disk on `Nvidia2` (RTX
5090, host driver 615.78.08). "Without the patch" is the same driver with
`FORCED_SAMPLE_PASSES=0`.

`tir_probe.exe` (x64), 93,467 covered pixels, three runs, each row measured
twice:

| Host GPU | Path, from the driver log | Pixels that differ from WARP | Pixels that differ between runs |
| --- | --- | --- | --- |
| `AMD1` | interlock, `hw=8 expand=16` | 24,281 and 24,208 | 484 and 389 |
| `AMD1` | passes | 0 and 0 | 0 and 0 |
| `Nvidia2` | interlock, `hw=16` native | 1,023 and 985 | 297 and 229 |
| `Nvidia2` | passes | 0 and 0 | 0 and 0 |

RADV has 8 samples at most, so on `AMD1` the interlock path draws 8 and
spreads them over the 16 bits. That is why a quarter of the pixels differ
from WARP there. The pixels that differ between runs are the fault.

Word 16.0.20625 (Current Channel, not activated), three starts on each path,
`vmlab vm screenshot` captures at 1024×768. The ribbon is the top 190 lines.
The numbers are the pixels that differ between two starts (1 and 2, 1 and 3,
2 and 3):

| Host GPU | Path | Ribbon pixels that differ | Of these by more than 2/255 |
| --- | --- | --- | --- |
| `AMD1` | interlock | 0, 1, 1 | 0, 0, 0 |
| `AMD1` | passes | 0, 0, 0 | 0, 0, 0 |
| `Nvidia2` | interlock | 461, 537, 353 | 436, 509, 323 |
| `Nvidia2` | passes | 1, 1, 0 | 0, 0, 0 |

On `Nvidia2` without the patch the gaps are in the outlines of the search
box, the font box and the Sign in, Comments and Editing buttons, as GitHub
#236 says. With the patch one pixel differs in one start, by 1/255 in one
channel: it is the window's backdrop, at (1015, 82), not a shape. With the
patch the ribbon on `Nvidia2` and the ribbon on `AMD1` have no pixel that
differs by more than 2/255.

What this says about the cause:

- The interlock path is not deterministic on the AMD host either: the probe
  finds 389 to 484 pixels that change between runs. So the fault is not
  NVIDIA's alone, and the design's class B ("a fault in the host path,
  suspected NVIDIA") does not hold as written. Two host drivers that share no
  code lose the same XOR, which points at how the driver uses the interlock,
  or at the Venus path both go through. Which of the two was not looked into.
- On `AMD1` Word's ribbon shows almost none of it: 1 pixel in one of three
  starts, against 353 to 537 on `Nvidia2`.

The acceptance run with the patch, on fresh `windows-11` labs:
`tests/e2e/run.sh` 12 of 12 on `AMD1` and 12 of 12 on `Nvidia2`, and
`tests/e2e/tir.sh` passes on both, x64 and x86 (0 of 93,467 pixels differ
from WARP, 0 between runs; the x64 driver log says `coverage=passes` for all
3 pipelines).

Word was installed in the lab with Microsoft's Office Deployment Tool from
Microsoft's CDN and is not part of any template. Its sign-in dialog was
closed with Alt+F4 before each capture.

The evidence (probe results, driver log lines, the Word captures) is on
helios in `/home/vmlab/src/stc/evidence/card-145/`.
