# The vblank from the host's frame clock

The driver's vsync came from a guest timer: a DPC every 16.7 ms that sent
the recorded flip and then an empty `SUBMIT_3D`, whose completion interrupt
raised `DXGK_INTERRUPT_CRTC_VSYNC`. Its period was the guest's timer
resolution and the guest's clock, and it cost one command per tick. This is
card #228, C11 of the design (docs/plans/win3d-real-adapter/DESIGN.md at the
root of the vmlab repository, option O2, stage 2, P2).

## The command

The feature bit of C10, `VIRTIO_GPU_F_VMLAB_ORDERED_FLIP` (bit 23), enables a
third command, `VIRTIO_GPU_CMD_VMLAB_VBLANK` (`0x0f02`), sent on the control
queue:

| Offset | Field | Meaning |
| --- | --- | --- |
| 0 | `virtio_gpu_ctrl_hdr` | Type `0x0f02`, no fence |
| 24 | `scanout_id` | 0, the only scanout |
| 28 | reserved | 0 |

The host holds the command until the next tick of its frame clock and then
answers `VIRTIO_GPU_RESP_OK_VMLAB_VBLANK` (`0x11f0`, in the range of OK
responses above those VIRTIO 1.2 defines), 48 bytes:

| Offset | Field | Meaning |
| --- | --- | --- |
| 0 | `virtio_gpu_ctrl_hdr` | Type `0x11f0` |
| 24 | `sequence` | The tick, counted from the start of the clock |
| 32 | `timestamp_ns` | The host's `CLOCK_MONOTONIC` time of the tick, in nanoseconds |
| 40 | `period_ns` | The period, 16,666,666 |
| 44 | reserved | 0 |

The clock ticks at 60 Hz, the rate of the mode in the device's EDID, from a
fixed start: tick n is at the start plus n/60 s, so it does not drift and
does not depend on when the guest asks. A wait is answered at the first tick
after it arrived, and no tick answers the same guest twice. The clock's
thread sleeps while no wait is held.

A held wait does not hold the queue. The commands behind it are served and
answered as they come, and the wait completes out of order at its tick. The
host refuses the wait (`ERR_INVALID_PARAMETER`) unless the driver acked the
bit, when it carries a fence, when its response buffer is shorter than 48
bytes, and on a device without a renderer, which keeps online snapshots and
so cannot hold a chain across one. A scanout other than 0 is refused with
`ERR_INVALID_SCANOUT_ID`. A reset of the device drops a held wait. A lost
host GPU ([device-lost.md](device-lost.md)) does not stop the clock: the wait
needs no host GPU, so the guest keeps its vsync.

The device is in `hv/crates/vmlab-hv-devices/src/virtio/gpu/vblank.rs` (the
clock and the answer) and `mod.rs` (`vblank_wait`, the hold in
`queue_notify`), at the root of the vmlab repository. The driver side is
`patches/kmd/kmd-host-vblank.patch`.

## The driver

With the bit acked, `StartVsyncTimer` starts no timer and keeps one wait on
the queue:

1. The interrupt routine sees the answer of the wait. When no flip is
   waiting to be sent, it raises the vsync, which reports the addresses
   whose flips were sent, as `0008` made it.
2. When a flip is waiting, the interrupt routine leaves the vsync to the
   completion of the wait. That completion, at DPC level, sends the flip as
   the timer did (`Flip()`), then raises the vsync through
   `DxgkCbSynchronizeExecution`, and then sends the next wait. The host
   serves the flip before it holds the wait.

So the vsync of every tick comes at the host's tick, a few microseconds
later when a flip goes out first, and a flip is reported only after it was
sent, as with the timer and its NOP after the flip. The vsync does not wait
for the host's answer to the flip. That answer comes once the host GPU has
shown the frame (the ordered flip of C10), so a vsync tied to it takes the
jitter of the host GPU ("Result" has the measurement).

`StopVsyncTimer` stops the resubmission. A wait still on the queue goes on
ticking when a start follows before its answer. A reset cancels the wait
with `ERR_UNSPEC`, and a start on a new transport sends a new one. The trace
build records each answer as event 25 (`build/kmd/README.md`).

A flip that Windows records between the interrupt of the wait and its
completion is sent by that completion and reported by the vsync of the next
tick. No tick raises two vsyncs.

The interrupt data of `DXGK_INTERRUPT_CRTC_VSYNC` has no timestamp, so the
host time reaches Windows only as the time of the interrupt. The driver
keeps the last tick and host time (`m_hostVblankSequence`,
`m_hostVblankTimeNs`) and the trace build records them. `GetScanLine` is
still not implemented.

## Without the bit

- A host that does not offer it (QEMU, a vmlab-hv before this change)
  leaves the driver on the timer and its NOP, unchanged by `0012`.
- A wait that the host refuses, or that the driver cannot queue, puts the
  driver back on the timer until the next start.
- A driver that does not know the bit does not send the command, and the
  host refuses it from a driver that did not ack the bit.

## Result

The unit tests of vmlab-hv show the hold, the answer and the refusals:
`a_vblank_wait_is_held_until_a_tick_without_holding_the_commands_behind_it`,
`a_vblank_wait_is_refused_without_the_bit_a_scanout_or_room_for_its_answer`,
`a_reset_drops_the_held_vblank_waits`,
`the_frame_clock_ticks_at_60_hz_from_its_start_and_never_twice` and
`the_clock_thread_answers_a_wait_and_each_next_one_a_period_later`.

### Live run

Windows 11 (`26100.1742.11`) on vmlab-hv of vmlab commit `35f40299`, driver
`100.6.101.58113`, on helios on 2026-11-21. Each GPU had a fresh lab of
`tests/e2e/run.sh --template windows-11 --host-gpu <gpu> --keep`, which
passed 12 of 12 Done checks, and then three runs of the adapter suite's
`pace` check (`tests/e2e/adapter.sh <lab> win <dir> pace`). The host kernel
logged no Xid. vmlab-hv logged "the guest takes its vblank from the host
frame clock" in each lab. The three values in a cell are the three runs.

| Run | Vblank interval, mean (ms) | p01 (ms) | p99 (ms) | Present rate (frames per second) |
| --- | --- | --- | --- | --- |
| AMD1, host vblank (`0012`) | 16.667, 16.667, 16.665 | 15.76, 16.50, 16.47 | 17.88, 16.81, 16.87 | 60.23, 60.21, 60.21 |
| AMD1, timer, the baseline of [acceptance.md](acceptance.md) (2026-11-20, one run) | 16.661 | 15.87 | 17.18 | 56.54 |
| Nvidia2, host vblank (`0012`) | 16.662, 16.662, 16.667 | 16.15, 15.13, 16.32 | 17.14, 17.30, 17.01 | 43.92, 43.84, 43.45 |
| Nvidia2, timer: driver `58112` of commit `f16e1372`, the same vmlab-hv, the same hour | 16.652, 16.647, 16.651 | 15.25, 15.28, 15.68 | 17.61, 17.97, 17.30 | 43.37, 44.77, 43.80 |

- The mode is 60 Hz, 16.667 ms. With the host's vblank the mean interval is
  the period on both GPUs, and its spread is smaller than with the timer:
  the standard deviation on Nvidia2 was 0.17 to 0.32 ms against 0.59 to
  0.76 ms.
- On AMD1 a vsynced present ran at the refresh rate, 597 presents in 597
  refreshes in each run. The baseline with the timer had 563 in 599.
- On Nvidia2 the present rate fails the suite's limit (57.0) with and
  without `0012`. The driver of `f16e1372`, which has no `0012`, gave the
  same 43 to 45 frames per second in the same hour. So this is not the
  vblank. The baseline of acceptance.md had 58.32 on 2026-11-20 at commit
  `d88304f2`: something merged since lowered it, and it is not found yet.
- `scan-line` fails as before: `GetScanLine` is not implemented.

A first form of `0012` raised the vsync of a tick with a flip at the host's
answer to that flip, when the frame is shown. On Nvidia2, with a second lab
loading the host, the interval then had p01 12.56 ms and p99 20.58 ms (one
run). The form above does not have this.
