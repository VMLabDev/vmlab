# The present copy on the host

Card #227 (C10, [flip.md](flip.md)) had the host order the flip, and frames
still tore at 90 percent host load. What it left is the copy of a present of
the Blt kind (windowed Direct3D 9, WPF, DirectDraw, blt-model DXGI, Desktop
Duplication). The kernel driver did it on a guest CPU, row by row, in
`VioGpuCommand::Run`, into a surface that the compositor reads on the host
GPU, and no fence orders a guest CPU against host GPU work. This is card
#229, C12 of the design (docs/plans/win3d-real-adapter/DESIGN.md at the root
of the vmlab repository, option O2, stage 2).

## The command

The feature bit of C10, `VIRTIO_GPU_F_VMLAB_ORDERED_FLIP` (bit 23), enables
a second command, `VIRTIO_GPU_CMD_VMLAB_PRESENT_COPY` (`0x0f01`). The driver
sends it on the control queue where the CPU copy was, ahead of the fenced
NOP of the present:

| Offset | Field | Meaning |
| --- | --- | --- |
| 0 | `virtio_gpu_ctrl_hdr` | Type `0x0f01`, no fence |
| 24 | `src_resource_id` | The source, a blob that the guest has mapped |
| 28 | `dst_resource_id` | The destination: a resource with a guest backing (a GDI surface), or a mapped blob (Desktop Duplication) |
| 32 | `src_offset` | Byte offset of the source image in its memory, 0 |
| 40 | `dst_offset` | Byte offset of the destination image, the scanout offset of a blob |
| 48 | `src_stride`, `dst_stride` | Row pitch of each |
| 56 | `mode` | `VIOGPU_BLT_COPY_MODE`: 1 bytes as they are, 2 red and blue swapped, 3 R10G10B10A2 to RGBA8, 4 to BGRA8 |
| 60 | `bpp` | Bytes per pixel, 4 for every conversion |
| 64 | `count` | Boxes, 1 to 64 |
| 68 | reserved | 0 |
| 72 | boxes | `count` of: source x and y, destination x and y, width, height |

The driver clips the boxes with the code of its CPU copy
(`VioGpuBltCopyRegion` records a box in place of copying it), so both paths
copy the same pixels. The host checks each box against the memory it has
and refuses the whole command when one leaves it, writing nothing.

The host, on its renderer thread:

1. waits until every fence on its renderer's timeline has signalled, the
   guest's fences on every context ring among them, and then for the fences
   that the kernel tracks on the dma-buf of either resource, where it has
   one;
2. copies from the renderer's mapping of the source blob into the
   destination's guest backing or blob mapping, converting pixels as the
   driver did;
3. answers `OK_NODATA`.

No other command runs while it copies, and the control queue serves no later
command before the answer, so the present's NOP, and with it the fence that
tells Windows the present is done, follows the copy.

The copy runs on a host CPU, not as GPU work. The destination of a GDI
surface is guest RAM, and a GPU copy would not be ordered against the Venus
work of other contexts any better: NVIDIA dma-bufs carry no implicit fences
(F10 of the design). What orders the copy is the host's wait and the held
queue.

The device is in `hv/crates/vmlab-hv-devices/src/virtio/gpu/copy.rs`
(layout, checks and conversion) and `renderer.rs` (`Backend::present_copy`),
at the root of the vmlab repository. The driver side is `patches/kmd/0010`.

## Without the bit

- A host that does not offer it (QEMU, a vmlab-hv before this change), or a
  command that the driver cannot queue, leaves the copy on the guest CPU as
  before.
- vmlab-hv refuses the command unless the driver acked the bit, and on a
  device without a renderer.
