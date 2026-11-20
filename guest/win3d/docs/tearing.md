# Torn frames on the console: the guest cannot order the flip

Card #134 reported bands of an earlier frame on the console of a Windows
guest while a WPF window animated and the host was busy. Card #143 (C7 of
the design, docs/plans/win3d-real-adapter/DESIGN.md at the root of the vmlab
repository) looked for the missing order on
vmlab-hv with a trace build of the kernel driver.

The result: every wait that the guest can make with the fences of
virtio-gpu is in place, and frames still tear at 90 percent host load. The
missing wait is one that the guest cannot make. Between a frame that the
guest calls finished and the host's read of it for the scanout, host GPU
work on the same surface still runs, and the guest has no fence that covers
it. The host must order the flip. That starts stage 2 of the design (C10,
a host-ordered flip command). `patches/kmd/0008` fixes one real fault in the
vsync order that the trace found, but it is not the cause of the tears.

## The probe

`tests/tearing/run.sh <lab-dir> <vm>` runs against a lab that is up
(`tests/e2e/run.sh --keep` leaves one):

- `Start-TearWindow.ps1` opens a WPF window (hardware Direct3D 9, a present of
  the Blt kind) with eight rectangles of eight colours. All eight move to the
  same x position on every frame.
- `rfbgrab.py` reads full frames from the VM's `vnc.sock` at 30 per second. A
  frame in which a row of a rectangle starts at another column than the other
  rows is a torn frame. The first ones are kept as PPM files.
- `--load 90 --cpus LIST` is the slow host of the issue: vmlab-hv and its
  render server are pinned to those CPUs, the render server runs at
  `SCHED_IDLE`, and `hog.py` keeps the CPUs busy for 90 percent of the time.
- `--full` changes the whole window on every frame, so that each present
  copies all of it.
- `--trace` and `--trace-on-tear` read the flip trace of a trace-build kernel
  driver (`build/kmd/README.md`, "Trace build").

The adapter suite (`tests/e2e/adapter.sh`, checks `tear-flip` and `tear-blt`)
has a shorter tearing check without host load. It did not show this fault on
the unpatched driver (0 torn of about 1,260 frames, `docs/acceptance.md`).
This probe is the long one with the load. It is not part of the suite.

## The fault before any change

Windows 11 (`26100.1742.11`) on vmlab-hv, driver `100.6.101.58105`, the
package of the November 2026 base, 1024x768.

| Host GPU | Load | Mode | Frames | Torn |
| --- | --- | --- | --- | --- |
| AMD1 | none | rectangles | 1,348 | 0 |
| AMD1 | 90 percent | rectangles | 3,594 | 0 |
| AMD1 | 90 percent | rectangles | 20,000 | 6 (3 events) |
| Nvidia2 | 90 percent | rectangles | 20,000 | 0 |
| AMD1 | 90 percent | `--full` | 5,979 | 4 |
| Nvidia2 | 90 percent | `--full` | 5,978 | 3 |

A torn frame shows the upper rectangles of one frame and the lower ones of
the frame before, or the reverse. One showed the old positions erased and the
new ones not drawn yet.

## Which waits were tested

The trace build (`patches/kmd/0007`, `build/kmd/README.md`, "Trace build")
records each submission, completion, present copy, flip and vsync with its
time. Each row below is a wait that a real video card has and that the
driver could be missing. Load is 90 percent, as in the issue.

| | Wait | How it was tested | Result |
| --- | --- | --- | --- |
| H1 | The present copy waits for the application's rendering on its host queue | Trace: is a submission of the application's ring outstanding when a present copy begins? | Holds. 0 of 1,544 copies. The user-mode driver sends `vkQueueSubmit` in a DMA buffer with a fence on ring 1 and waits for it before the present |
| H2 | The flip waits for DWM's composition on its host queue | Trace: time from the completion of DWM's ring-1 DMA buffer to `SetVidPnSourceAddress` | Holds. The flip follows by about 30 µs, every time |
| W3 | DWM reads the destination of a present copy only after the copy | A sequence number around the copy, read by the user-mode driver around its upload. Knob 1 holds every copy for 2 ms when it is half done | Not the place. No process read the sequence number, so DWM takes the destination on another path. With the stall (copy median 3.2 ms, from 0.2 ms) 0 torn of 2,695 frames on AMD1 and 0 of 2,694 on Nvidia2. The patch is not in this repository |
| W4 | A vsync reports a flip only after the flip was sent to the host | Trace events 23 and 24, and knob 2, which brings back the old order | A real fault, fixed by `0008` (below). Not the cause: old order and new order both 0 torn in 300 s per GPU on the trace build, and the release build with `0008` still tears |
| W5 | A present copy waits until every earlier DMA buffer of the engine has retired, as one hardware engine would order it | A release build (`100.6.101.58108`, not in this repository) whose copy waits up to 8 ms for that | Not the cause. 4 torn of 20,000 frames on AMD1, 1 of 20,000 on Nvidia2 |

The trace also showed that 0.3 to 0.6 percent of present copies begin while
a DMA buffer of DWM is still pending on the host. W5 closed that gap, and
the tears stayed.

## Runs at 90 percent load

Whole-window mode (`--full`) unless the row says rectangles. A torn frame is
counted once for each grab that shows it.

| Driver | Build | Host GPU | Mode | Frames | Torn |
| --- | --- | --- | --- | --- | --- |
| `58105`, none of these patches | release | AMD1 | rectangles | 20,000 | 6 (3 events) |
| `58105` | release | Nvidia2 | rectangles | 20,000 | 0 |
| `58105` | release | AMD1 | full | 5,979 | 4 |
| `58105` | release | Nvidia2 | full | 5,978 | 3 |
| with W4 | release | AMD1 | rectangles | 20,000 | 0 |
| with W4 | release | Nvidia2 | rectangles | 20,000 | 0 |
| with W4 | release | AMD1 | full | 8,936 | 3 |
| W4 old order, knob 2 | trace | AMD1 | full | 8,980 | 0 |
| W4 old order, knob 2 | trace | Nvidia2 | full | 8,979 | 0 |
| with W4 | trace | AMD1 | full | 8,980 | 0 |
| with W4 | trace | Nvidia2 | full | 8,972 | 0 |
| with W4 and W5 (`58108`) | release | AMD1 | full | 20,000 | 4 |
| with W4 and W5 (`58108`) | release | Nvidia2 | full | 20,000 | 1 |
| `58109`, the package: W4 on the base with `0006` | release | AMD1 | full | 20,000 | 1 |

The rectangles mode tears too seldom to tell a change from chance: about
three events in 20,000 frames before any change. The whole-window mode
copies the whole window on every present and tears more often.

No trace build tore, in about 47,000 frames of whole-window mode in eight
runs, while release builds tore in both modes on both GPUs. The records
change the timing of the submit and interrupt path by a few microseconds,
and that is enough. So the trace build can rule a wait in or out only where
it records the order itself (H1, H2, W4). A torn count of 0 on a trace build
proves nothing.

## What the torn frames show

- Most torn frames split the window at one row: the rows above are of one
  frame and the rows below of the other, sometimes the reverse. A split once
  fell inside a row. One frame showed the old positions erased and the new
  ones not drawn yet.
- A torn frame stays for two grabs at 30 per second, so the display kept
  it for a whole frame of the guest.
- The `58108` run, with W5, showed frames with more than one split, and one
  frame in which single rows of one frame sit every fourth row inside the
  other. A linear copy that a reader overtakes gives one split. Rows in a
  pattern of four come from GPU work, which writes and reads in tiles.

So one side of the race is host GPU work that the guest does not see as
outstanding. Every completion that the guest waits for is a fence of a
context ring, and that fence says the host renderer is done with the
submission. It says nothing about other host work on the same surface:

- NVIDIA dma-bufs carry no implicit fence (F10 of the design), so a blob
  that two host contexts use has no order between them.
- vmlab-hv publishes a blob scanout to its display without an acquire fence
  (`acquire: None` in `dma_frame` of
  hv/crates/vmlab-hv-devices/src/virtio/gpu/renderer.rs), and its display reads the frame later, on its own thread.
  It waits for its readers of the last frames before the next submission,
  but no fence orders that read after GPU work on the surface that the host
  had already started.
- A vsync of this driver comes from a guest timer and a NOP on the control
  queue (F6). It tells Windows that the host has the flip, not that the host
  has read the frame or stopped reading the one before.

Which host operation it is, the display's read of the scanout or the
compositor's GPU work on a surface that the copy writes, the guest cannot
tell, because no record of the guest marks it. That is the answer for C7:
the order must come from the host, which sees both sides. A host-ordered
flip (C10) waits on the host for the GPU work on the new scanout before the
display reads it, and reports the flip back when the old scanout is free.
A present copy on the host GPU (C12) puts the copy on the same host queue as
the work around it.

## The vsync fix, `patches/kmd/0008`

The trace found one fault in the vsync order of the driver. It does not end
the tears, but Windows relies on the order, and the fix passed the win3d e2e
on both GPUs, so the package carries it.

The kernel driver has no display interrupt. A 60 Hz timer plays that part
(`viogpu_vidpn.cpp`, `viogpu_adapter.cpp`):

1. `SetVidPnSourceAddress` records the new primary, puts its address into a
   queue and sets `m_shouldFlip`. Windows calls it at any time.
2. The timer DPC calls `Flip()`: when `m_shouldFlip` is set, it sends
   `SET_SCANOUT_BLOB` and `RESOURCE_FLUSH` for the recorded primary. Then it
   sets `m_vsync` and sends a NOP to the host.
3. The interrupt routine, on the next interrupt with `m_vsync` set, takes the
   next address from the queue and reports `DXGK_INTERRUPT_CRTC_VSYNC` with
   it.

A `SetVidPnSourceAddress` between step 2 and step 3 queued an address whose
flip the timer had not sent, and step 3 reported it all the same. Windows
takes a reported address as shown, so it gave the buffer still on screen
back to DWM 16 ms early. The trace counted 2 such reports in 20 s on AMD1
at 90 percent load. The gap between step 2 and step 3 is the round trip of
the NOP, microseconds on an idle host and milliseconds on a loaded one.

The fix, driver `100.6.101.58109`:

- `Flip()` takes the primary, its address and the tail of the queue as one
  set. After the flip is on the control queue it publishes how far the queue
  was sent.
- The interrupt routine reports only addresses below that mark. With none
  left it reports the address of the last flip sent, not the newest one
  recorded.
- `SetVidPnSourceAddress` runs at any IRQL, also inside the interrupt
  routine, so a lock cannot make the set whole. It counts a sequence number
  up before and after its change, and `Flip()` reads again when the number is
  odd or has changed. A first version with a spin lock hung the guest at
  logon.

A held-back address is reported one tick later, with the vsync that follows
its flip. The trace build counted 3 and 4 held back in 20 s.

## What is left

- C10 of the design: a feature bit and a host-ordered flip command.
- The probe needs a host at 90 percent load and several minutes, and the
  fault is not at 0, so it stays a script (`tests/tearing/run.sh`) beside
  the e2e tests. The adapter suite keeps its short tearing checks
  (`tear-flip`, `tear-blt`) without load. When C10 lands, a run of this probe
  at load is its check.
- vmlab-hv ended with "Too many open files" after about 40 s of the probe on
  Nvidia2 at the default limit of 1024 descriptors: it holds one dma-buf
  descriptor for each blob resource. The runs here raised the limit
  (`ulimit -n`) before `vmlab up`. That is a fault of vmlab-hv, outside this
  card.
