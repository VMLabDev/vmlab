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

- C10 of the design, a feature bit and a host-ordered flip command, is
  built ([flip.md](flip.md)). It did not end the tears: 2 torn of 20,000 on
  AMD1 and 2 of 20,000 on Nvidia2 in whole-window mode at 90 percent load,
  the rate of the runs above. What that leaves is in flip.md, "Result".
- C12, the copy of a Blt present on the host behind the same bit, is built
  ([present-copy.md](present-copy.md)). It did not end the tears either:
  2 torn of 20,000 on AMD1 and 2 of 20,000 on Nvidia2 in the same probe.
- With C10 and C12 together the rate is 2 torn of 20,000 frames at
  90 percent load on both GPUs. That rules out the flip order (C10) and the
  present copy (C12) as the cause. What is left is host work that no fence
  covers, and the guest cannot see which work that is. Finding it needs a
  host-side trace, which is new work beyond C12.
- The probe needs a host at 90 percent load and several minutes, and the
  fault is not at 0, so it stays a script (`tests/tearing/run.sh`) beside
  the e2e tests. The adapter suite keeps its short tearing checks
  (`tear-flip`, `tear-blt`) without load.
- vmlab-hv ended with "Too many open files" after about 40 s of the probe on
  Nvidia2 at the default limit of 1024 descriptors: it holds one dma-buf
  descriptor for each blob resource. The runs here raised the limit
  (`ulimit -n`) before `vmlab up`. That is a fault of vmlab-hv, outside this
  card.

## The host-side trace (card #233)

Card #233 traced on the host what touches a torn frame, to name the host
work that no fence covers. The effort was bounded to two sessions. This is
the result of the first.

### The trace

vmlab-hv writes the trace when its environment sets `VMLAB_HV_GPU_TRACE` to
a directory. It is off by default. Each process appends one line per event
to `<dir>/gpu-trace-<pid>.txt`, with the time on `CLOCK_MONOTONIC`
(`hv/crates/vmlab-hv-display/src/trace.rs`, at the root of the vmlab
repository). The lines are buffered and written every 100 ms.

| Event | Where | What it records |
| --- | --- | --- |
| `cmd` | renderer thread | Every control command: type, context, fence flags, fence id, ring, length, duration |
| `fence_new`, `fence_done` | renderer timeline | Each host fence with the guest fence it carries (context, ring, id), and when it signals |
| `order` | ordered flip | Fences pending before the host wait, the wait's duration, the dma-buf writers of the scanout |
| `pcopy`, `pcopy_src` | present copy | Source and destination, fences pending, the wait, the dma-buf fences of both; rows of the source that changed while it was copied |
| `publish`, `export` | display publish | The publication time, the scanout resource, direct or copied, its modifier and dma-buf fences |
| `readback` | console readback | The resource, its dma-buf writers, a digest of the pixels |
| `rfb_send` | RFB server | The publication that each RFB update sent |
| `flip` | ordered flip | The guest frame number and resource |

`VMLAB_HV_GPU_TRACE_REREAD=1` also reads each readback twice, and the
present copy's source again 2 ms after the copy. It changes the timing more.

`tests/tearing/rfbgrab.py` now records the `CLOCK_MONOTONIC` times of each
torn grab, and `tests/tearing/hosttrace.py <trace> <grab.json>` takes each
torn grab to the RFB update inside it, the publication that update sent,
that publication's export and readback, and every event of the 50 ms before.

### Runs

The labs of card #229 (driver `58111`, the host-ordered flip and the host
present copy), vmlab-hv of this card, whole-window mode, 90 percent load,
20,000 frames.

| Run | Host GPU | Trace | Torn |
| --- | --- | --- | --- |
| rr | AMD1 | on, with the reread | 0 |
| t1 | AMD1 | on | 4 (frames 14815, 15602, 19648, 19894) |
| t2 | AMD1 | on, with the source check of the present copy | 7 (5 events: frames 5037, 9583, 15446, 17116, 18139) |
| t1 | Nvidia2 | on, with the source check | 0 |

Run t1 on AMD1 also counted 79 frames after frame 20449 in which the lowest
rectangle was missing or moved as a whole: something covered the window at
the end of the run. They are not counted as torn. No Xid on either GPU.

The trace without the reread did not stop the tears on AMD1: 11 torn
frames (9 events) in 40,000, more than the 2 in 20,000 of card #229. The reread run and
the Nvidia2 run did not tear, so the tears there say nothing.

### What the trace shows

The scanouts of this guest are three resources (24, 25 and 26 on AMD1) with
the modifier `DRM_FORMAT_MOD_INVALID`, so vmlab-hv cannot hand them to the
display as they are. Every publication is a GL copy of the scanout into a
pool buffer, and each flip publishes twice: at `SET_SCANOUT_BLOB`, and again
in the ordered flip, after its host wait.

- T1. The torn image is in the scanout before the flip and does not change
  after it. For every torn grab the publication sent was one of the two
  copies of its flip, and the two copies of each flip had equal digests:
  16,172 of 16,172 pairs (AMD1 rr), 17,915 of 17,915 (AMD1 t2), 15,336 of
  15,336 (Nvidia2). The display path does not make the tear. GPU work in
  the guest composed it into the frame before the flip.
- T2. The source of the present copy, the application's back buffer, did
  not change while the host copied it: 0 rows in 17,306 copies on AMD1 and
  in 15,368 on Nvidia2. The application's rendering does not race the copy.
- T3. When an ordered flip or a present copy waited, no fence was pending
  on the renderer's timeline before 99 percent of them (AMD1 rr: 19,309
  of 19,376 flips, 17,903 of 18,054 copies). The kernel's dma-buf fences
  of the scanouts and destinations read busy once in about 35,000 exports
  (AMD1 t2) and never in the other runs. The host waits had almost nothing
  to wait for.
- T4. The fences of ring 0 of the Venus contexts are not GPU fences. The
  application's context and DWM's send three `SUBMIT_3D` per frame of 24
  bytes, the size of `vkNotifyRingMESA`, each with a fence on ring 0. That
  fence goes to virglrenderer's global timeline, which no Venus work
  blocks: it signals about 20 us after it was made (median on AMD1, about
  100 us on NVIDIA). The fences on ring 1, after a `vkQueueSubmit`, take 0.7 ms
  (application) and 1.5 ms (DWM) on AMD1, 1.8 ms and 2.0 ms on NVIDIA.

### What is left

The trace does not name the work. What it leaves is narrower than before:
the tear is composed by guest GPU work into the scanout, from inputs that
were complete when the host wrote them. The candidate that fits T1 to T4 is
DWM's composition reading the window's redirection surface (the destination
of the present copy) while the host present copy writes it, or the reverse.

- Venus commands reach the render server through the context's ring in
  shared memory. The render server runs them when its ring thread reads
  them, as soon as the user-mode driver writes them, and not when the
  guest's scheduler runs the DMA buffer that holds the `vkNotifyRingMESA`.
  So the order that the guest's scheduler gives the present copy and DWM's
  next composition does not hold on the host.
- That DMA buffer completes on a ring-0 fence, which signals at once (T4).
  The present copy's host wait covers fences on the timeline only (T3), so
  it cannot wait for GPU work that is reached through the ring.
- A linear CPU copy that a tiled GPU read overtakes gives a split at one
  row, with groups of one to four rows at the edge. That is what the torn
  frames show.

vmlab-hv does not see when the render server reads a ring or when the GPU
runs that work, so this trace cannot prove it. The next step, for the
second session of the card: record in `virgl_render_server` the time at
which the ring thread decodes each `vkQueueSubmit`, with its context and
the resources of its command buffers, into the same trace. Then find, in a
torn frame, DWM's submission that read the destination of the present copy
and see whether it ran during that copy.

## The render server's submissions (card #237)

The second and last session of the host trace. It recorded every Venus
queue submission in the render server against the torn frames. The trace
does not show unordered host work, and the investigation ends here.

### The trace

The diagnostic patch
[0041](../../../patches/virglrenderer/diagnostic/0041-vkr-trace-queue-submissions-for-the-gpu-trace.patch)
of virglrenderer 1.3.0 (at the root of the vmlab repository, not applied by
the build) makes each render worker write to the trace of vmlab-hv when
`VMLAB_HV_GPU_TRACE` is set. Each worker writes its own
`gpu-trace-<pid>.txt` in the same format and on the same clock:

| Event | What it records |
| --- | --- |
| `vk_submit` | Every `vkQueueSubmit` and `vkQueueSubmit2`, at the time the ring thread hands it to the host driver: context, queue, fence, the wait and signal semaphores of each batch, its command buffers, and the virgl resources that they use (`w` attachment or copy destination, `r` copy source, `d` descriptor, `b` barrier) |
| `vk_done` | When an empty submission with a fence of the trace, made on the same queue just after it, signals. The host GPU has finished the work by then |

The resources come from the objects of the context: memory imported from a
resource or exported as one, the images and buffers bound to it, views,
framebuffers and descriptor set slots. `vk_done` is an upper bound. Its
waiter thread runs at `SCHED_IDLE` under the probe's load, like the rest of
the render server.

`tests/tearing/hosttrace.py <hv trace> <grab.json> --vk <worker traces>`
then reports the compositions (submissions that write a scanout), the GPU
writes before and after each present copy into its source, and for each
torn frame its composition and the copy that fed it.
`--alias DST=RES` names the resource that a context reads where a copy
writes DST (see below).

### Runs

The labs of card #229 (driver `58111`), vmlab-hv of card #233 with this
render server, whole-window mode, 90 percent load, 20,000 frames.

| Run | Host GPU | Torn | Events |
| --- | --- | --- | --- |
| v1 | AMD1 | 3 grabs (frames 8366, 17969, 17970) | 2 |
| v1 | Nvidia2 | 2 grabs (frames 7169, 7170) | 1 |

Each event shows the upper rectangles of one frame and the lower ones of
the other, as before. Run v1 on Nvidia2 also counted 48 grabs after frame
17639 in which only the lowest rectangle was wrong or missing. That is the
window being covered at the end of the run, as in run t1 of card #233, and
it is not counted. No Xid on either GPU. With the trace on, both GPUs
still tore, so this trace did not hide the fault.

### What the trace shows

On AMD1 the present copy always goes from resource 284 (the application's
back buffer, context 30) to 287. Resource 287 has a guest backing and is
attached to no Venus context. DWM (context 6) composes each frame by a copy
from resource 62 into the scanout (24, 25 or 26) and into resource 20. On
Nvidia2 the resources are 318, 321 and 160.

- S1. No GPU work writes what DWM composes from. 0 of 70,082 submissions
  on AMD1 and 0 of 61,052 on Nvidia2 write 62 or 160. No submission uses
  287 or 321 at all. The memory that DWM reads is written only by a CPU.
- S2. DWM reads it once for each present copy. 22,292 compositions read 62
  for 22,375 copies on AMD1, and 19,365 read 160 for 19,420 copies on
  Nvidia2. The analysis takes 62 and 160 as the memory that the copy
  writes (`--alias 287=62`, `--alias 321=160`). The trace cannot show the
  guest pages behind 287, so this is an assumption. If it is wrong, the
  copy destination is read by no host GPU work at all, and S1 still holds.
- S3. The composition of each torn frame began 6 to 49 ms after the copy
  that fed it ended: 17.1 ms (frame 8366) and 6.1 ms (frames 17969 and
  17970, one composition) on AMD1, and 48.9 ms (frames 7169 and 7170) on
  Nvidia2. It ran for 1.2 to 4.2 ms, and no copy ran during it. Over all
  compositions, 73 of 23,702 (AMD1) and 48 of 20,559 (Nvidia2) overlapped a
  copy. None of them was a torn frame. The candidate of card #233, DWM's
  composition reading the destination while the host copy writes it, did
  not happen in any torn frame.
- S4. The source was complete before each torn frame's copy. The
  application's last submission that wrote it was done 3.8 ms and 0.03 ms
  before the copy began on AMD1, and 2.4 ms before it on Nvidia2. For the
  0.03 ms case, the guest's ring-1 fence of that submission had signalled
  0.9 ms before the copy began, so the work was done. In 0 of 41,795 copies
  was the next write into the source submitted before the copy ended. 185
  copies on AMD1 and 170 on Nvidia2 began before the `vk_done` of the last
  write. None of them fed a torn frame. They are within the latency of the
  trace's waiter.
- S5. The torn frames' copies took 2.25 ms and 3.88 ms on AMD1, longer
  than 90 and 99.6 percent of all copies, and 0.95 ms on Nvidia2, longer
  than 62 percent of them. Three events cannot show a link between a long
  copy and a tear.

### Result

The trace does not show the unordered work. It traced each queue
submission of every Venus context, with its resources and an upper bound
of its GPU time, together with the present copies and publications of
vmlab-hv. In each torn frame, the source was finished before the copy
began, no submission changed it during the copy, and the composition read
the destination several milliseconds after the copy ended, with no copy
running. No host GPU work writes the memory that DWM composes from. So no
host work that the trace can see makes the tear. What is left is outside
the host's view: guest CPU writes into the memory that DWM reads, or the
guest's order of them. vmlab-hv and the render server cannot record those.
This was the second of the two bounded sessions, and the investigation of
#134 ends with this result.
