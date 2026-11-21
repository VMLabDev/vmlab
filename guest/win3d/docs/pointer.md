# Hardware pointer

## Cause

Up to driver `100.6.101.58105` the kernel driver reported no pointer
capability: the block that sets `MaxPointerWidth` and `MaxPointerHeight` was
commented out in every Yttrium release, and `DxgkDdiSetPointerShape` and
`DxgkDdiSetPointerPosition` returned `STATUS_NOT_IMPLEMENTED`. Windows
therefore drew the pointer into each frame, and it reached the host's screen
with the frame, a few frames after the hand moved (card #137, GitHub #235).

## Fix

Patch `kmd-hardware-pointer.patch` implements both calls, and the INF
sets `HWCursor` to 1 in the adapter's class key. `HWCursor` = 0 gives the
old behaviour back after a restart of the device. The driver version is
`100.6.101.58106`.

- **Shape.** Windows' pointer, in any of its three forms (monochrome, colour,
  masked colour), becomes a 64×64 ARGB image in a 2D resource.
- **Commands.** `VIRTIO_GPU_CMD_UPDATE_CURSOR` sets the image and shows or
  hides the pointer, with resource id 0 for hidden.
  `VIRTIO_GPU_CMD_MOVE_CURSOR` only moves it. vmlab-hv reads the resource id
  on an update only, so a hide sent as a move left the pointer on the screen.
- **Position.** Windows gives the top left corner of the image. The driver
  sends that plus the hotspot, the position of the hotspot, because
  vmlab-hv passes the position on unchanged and its frame protocol defines
  it as the hotspot's (`CursorState`). With the corner, the I-beam was 8 and
  9 pixels off. A hotspot is never left of or above the screen, so nothing
  is clamped at an edge.
- **Order.** The image is uploaded on the control queue and the commands are
  on the cursor queue, which the host reads independently. The driver waits
  up to 250 ms for a no-op queued behind the upload before it sends the
  command.
- **Reset.** A device reset (timeout recovery, D3 to D0) destroys the host's
  resource. `RestoreHostObjects` makes it again under the same id and sends
  the last shape, hotspot and visibility again.

The host needs no change: vmlab-hv already serves the cursor queue and
passes shape and state to its frame protocol, to RFB (`vnc.sock`), to the
viewer and to the recorder.

## Limits

- **Inverting pixels.** A host pointer cannot invert the screen under it,
  which the monochrome and masked-colour forms ask for (the text I-beam).
  Those pixels are black, with a white outline on the transparent pixels
  beside them.
- **64×64.** Windows draws a larger pointer (accessibility sizes, high
  display scaling) into the frame itself, as before.
- **One display.** Only VidPn source 0 has a hardware pointer.
- **Position convention.** The virtio-gpu specification does not say which
  point of the image the position names. A host that takes it as the top
  left corner shows this driver's pointer too far right and down by the
  hotspot. Not tested on QEMU.

## Check

`tests/e2e/pointer.sh <lab-dir> <vm> <evidence-dir>` runs against a lab on
vmlab-hv where the package is installed and the autologon desktop is up,
for example one kept with `tests/e2e/run.sh --keep`.
`tests/guest/Show-TestPointer.ps1` shows a flat full-screen window in the
user's session and steps the pointer through arrow, I-beam, resize, hidden
and arrow, each at its own position. `tests/e2e/pointer.py` records the
pointer on vmlab-hv's native frame protocol (`display.sock`) and on RFB
(`vnc.sock`) meanwhile, and the script captures the console with
`vmlab vm screenshot` and records it with `vmlab record`. The checks:

| Check | Passes when |
| --- | --- |
| `shapes` | Every step arrives on the frame protocol in order, as the right kind of shape with its hotspot at the step's position, and the pointer is hidden in the hidden step |
| `rfb` | RFB carries a cursor rectangle for each kind, and an empty one for hidden |
| `outline` | The I-beam has only black and white opaque pixels, more white than black |
| `frame` | At least three captures show the test window, and none has anything but the background where a step put the pointer |
| `recording` | Frames of the video show a pointer at each visible step's position, including both arrow steps, and none at the hidden step's position |

The viewer draws from the same frame protocol messages the `shapes` check
reads. No check looks at a viewer window.

The script is not one of the 12 Done checks of `tests/e2e/run.sh`. The
adapter suite (`tests/e2e/adapter.sh`, which `run.sh --apps` ends with) runs
it as its check `pointer-shapes`, next to `pointer`, which looks only at
whether the pointer is in the frame
([acceptance.md](acceptance.md#adapter-suite)).

## Verification

The 2026-11-20 recording check excluded both arrow steps. On helios, with
vmlab-hv built from `next`, the package built at
`de4169bd` (driver `100.6.101.58106`, `HWCursor` 1 in the class key), a fresh
`windows-11` lab for each host GPU:

| Host GPU | `tests/e2e/run.sh` | `tests/e2e/pointer.sh` |
| --- | --- | --- |
| `AMD1` | 12 of 12 | 5 of 5 |
| `Nvidia2` | 12 of 12, no Xid in the kernel log | 5 of 5 |

In both runs the frame protocol held the arrow at (200,300) with hotspot
(0,0), the I-beam at (320,300) with hotspot (8,9), the resize pointer at
(440,300) with hotspot (11,4), nothing in the hidden step and the arrow at
(680,300), which are the positions the guest script set. RFB carried all four
kinds. The I-beam had 26 black and 54 white pixels. 31 captures showed the
test window and none had a pointer in it.

A first build sent the image's top left corner and hid with a move. The
check failed it on both: the I-beam was reported at (312,291), and the
hidden step still had a visible arrow.

Not run: the check against the package before this patch, more than one
display, a pointer above 64×64, and a device reset while the pointer is on
(`RestoreCursor` is covered by the compile only).
