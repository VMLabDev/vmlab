# Stale bands on the console: a vsync that reported a flip too early

Card #134 reported bands of an earlier frame on the console of a Windows
guest while a WPF window animated and the host was busy. Card #143 looked for
the missing order on vmlab-hv. This page says what was measured, what the
cause was not, what it was, and what `patches/kmd/0007` changes.

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

## Before the fix

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

## What it was not

The design of card #141 gave two hypotheses. The trace build
(`patches/kmd/0006`) answered both.

- **H1, the present copy reads the application's rendering too early.** No.
  In 1,544 present copies none began while a submission of the application's
  host queue was outstanding. The user-mode driver sends `vkQueueSubmit` in
  the DMA buffer with a fence on ring 1, after a wait for the ring.
- **H2, the flip goes out before the compositor's rendering is finished.**
  No. `SetVidPnSourceAddress` followed the completion of DWM's DMA buffer by
  about 30 µs in every case.

A third idea was that DWM reads the destination of a present copy while the
kernel driver writes it on a guest CPU. A first patch for it (a sequence
number around the copy, read by the user-mode driver around its upload) is
not in this repository, for two reasons:

- Its trace record showed that no process ever read the sequence number: DWM
  does not take the destination through that upload path.
- Knob 1 of the trace build holds every present copy for 2 ms when it is half
  done. The copy then lasts 3.2 ms (median, from 0.2 ms). A reader that takes
  the destination at any time would tear in about one frame of ten. The
  result at 90 percent load, `--full`, 90 s: 0 torn of 2,695 frames on AMD1
  and 0 of 2,694 on Nvidia2. So Windows keeps the compositor's read after the
  copy, and the copy is not the place.

## The cause

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

A `SetVidPnSourceAddress` between step 2 and step 3 queues an address whose
flip the timer has not sent. Step 3 reported it all the same. Windows takes a
reported address as shown: the flip is complete, and the buffer that was
shown before is free. But the host shows that buffer until the next tick,
16 ms later. DWM renders the next frame into it, and the console reads a
frame that is half rendered. That is the band: rows that DWM has drawn
already beside rows of the frame that the buffer held.

The gap between step 2 and step 3 is the round trip of the NOP. On an idle
host it is some microseconds. On a host at 90 percent load, with vmlab-hv
waiting for a CPU, it is milliseconds. That is why the fault needs load.

Both hypotheses of the design ask whether a flip waits for the rendering
before it. This fault is the other order: the rendering of the next frame
does not wait for the flip.

## The fix

`patches/kmd/0007-kmd-report-flips-after-they-are-sent.patch`, driver
`100.6.101.58107`:

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
its flip. Windows already expects that of a flip.

The guest waits for no host fence here. The control queue keeps the order:
the flip is ahead of every command that the next frame sends.

## After the fix

RESULTS_PLACEHOLDER

## What is left

- The check needs a host at 90 percent load and several minutes. It is a
  script (`tests/tearing/run.sh`), not a check of the adapter suite.
- vmlab-hv ended with "Too many open files" after about 40 s of the probe on
  Nvidia2 at the default limit of 1024 descriptors: it holds one dma-buf
  descriptor for each blob resource. The runs here raised the limit
  (`ulimit -n`) before `vmlab up`. That is a fault of vmlab-hv, outside this
  card.
