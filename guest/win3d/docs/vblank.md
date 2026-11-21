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
`patches/kmd/0012`.

## The driver

With the bit acked, `StartVsyncTimer` starts no timer and keeps one wait on
the queue:

1. The interrupt routine sees the answer of the wait. When no flip is
   waiting to be sent, it raises the vsync, which reports the addresses
   whose flips were sent, as `0008` made it.
2. The completion of the wait, at DPC level, sends the waiting flip as the
   timer did (`Flip()`), and then the next wait. The host serves the flip
   before it holds the wait.
3. A flip sent this way carries `FlipShownDone` on its flush. The host
   answers the flush of a flip, the ordered flip of C10, once the frame is
   on its display, and that answer raises the vsync of the tick. So the
   address of a flip is reported when it is shown, as with the timer and
   its NOP after the flip.
4. When the answer to that flip has not come by the second vblank after it
   (a flush that never reached the host), the vblank raises the vsync
   itself, so the vsync cannot stop.

`StopVsyncTimer` stops the resubmission. A wait still on the queue goes on
ticking when a start follows before its answer. A reset cancels the wait
with `ERR_UNSPEC`, and a start on a new transport sends a new one. The trace
build records each answer as event 25 (`build/kmd/README.md`).

One case gives two vsyncs in one tick: a flip that Windows records between
the interrupt of the wait and its completion. The interrupt raised the vsync
because no flip was waiting, and the completion sends the new flip, whose
answer raises a second one. Windows takes a second vsync with a newer
address as a frame shown early.

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
