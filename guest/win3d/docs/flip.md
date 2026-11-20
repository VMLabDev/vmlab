# The host-ordered flip

Card #143 found that no wait the guest can make ends the torn frames of
card #134 under host load ([tearing.md](tearing.md)). Between a frame that the
guest calls finished and the host's read of it for its display, host GPU
work on the same surface can still run, and no fence of the guest covers it.
vmlab-hv sees every host queue, so it orders the flip itself. This is card
#227, C10 of the design (docs/plans/win3d-real-adapter/DESIGN.md at the root
of the vmlab repository, option O2, stage 2).

## The feature bit and the command

vmlab-hv's virtio-gpu offers one private feature bit,
`VIRTIO_GPU_F_VMLAB_ORDERED_FLIP`, bit 23. VIRTIO 1.2 gives bits 0 to 23 to
the device type and defines 0 to 5 for virtio-gpu, so the top bit is the
furthest from the next standard one. With the bit acked, the driver sends
`VIRTIO_GPU_CMD_VMLAB_ORDERED_FLIP` (`0x0f00`, outside the 2D, 3D and cursor
ranges) in place of `RESOURCE_FLUSH` for every flush of the scanout:

| Offset | Field | Meaning |
| --- | --- | --- |
| 0 | `virtio_gpu_ctrl_hdr` | Type `0x0f00`. A fence flag is allowed, as on any command |
| 24 | `scanout_id` | 0, the only scanout |
| 28 | `resource_id` | The resource that `SET_SCANOUT_BLOB` or `SET_SCANOUT` made the scanout |
| 32 | `rect` | x, y, width, height of the flush |
| 48 | `frame` | The driver's frame number, counted up for each flip |

The host answers `OK_NODATA`, and only after these steps, in this order:

1. It waits until every fence on its renderer's timeline has signalled,
   whatever guest process and context it belongs to, and then until the
   writers that the kernel tracks on the scanout's dma-buf are done (its
   implicit fence, `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`). A guest sees only the
   fences of its own submissions, so it cannot make this wait. NVIDIA
   attaches no implicit fence, so there the second wait returns at once.
2. It records the flush, publishes the frame to its display, and waits until
   the console has read it (the readback that RFB on `vnc.sock` and the
   recorder use runs on the renderer).
3. It waits until no display reader holds an older frame.

A first version also put a host fence on every context ring that the guest
had fenced. virglrenderer refuses a fence on a ring whose queue the guest
has destroyed (`vkr: submit_fence: invalid ring_idx`), and the render server
then ends that Venus context: on Nvidia2 the Direct3D 12 checks of the e2e
run lost their device. The host does not fence the guest's rings.

The control queue serves no later command before the answer. The vsync of
the driver comes from the NOP that its timer sends after the flip, and the
completion of a flip from a DMA buffer is the answer itself, so Windows
learns that the old primary is free only after the host has shown the new
one and stopped reading the old one.

The device is in `hv/crates/vmlab-hv-devices/src/virtio/gpu/mod.rs`
(`ordered_flip`) and `renderer.rs` (`Backend::order`), at the root of the
vmlab repository. The driver side is `patches/kmd/0009`.

## Without the bit

- A driver that does not know the bit (Linux, the virtio-win drivers, a
  viogpu3d before driver `100.6.101.58110`) does not ack it. vmlab-hv refuses
  the command then, and the driver's `RESOURCE_FLUSH` works as before.
- A host that does not offer it (QEMU, a vmlab-hv before this change) leaves
  the driver on `RESOURCE_FLUSH`. That is the standard path of the driver,
  unchanged by `0009`.
