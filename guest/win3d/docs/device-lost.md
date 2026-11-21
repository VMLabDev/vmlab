# A lost host GPU

A host GPU can go away under a running guest: the render server that runs
every Venus context ends, or the GPU's render node goes with its driver.
Before card #230 vmlab-hv kept waiting on it, the guest's fences never
signalled, and the kernel driver's timeout recovery waited for host objects
the host could not make, timeout after timeout. This is card #230, C13 of
the design (docs/plans/win3d-real-adapter/DESIGN.md at the root of the vmlab
repository, option O2, stage 2, property P11): a lost host GPU is a removed
device in the guest and not a dead VM.

## The host

vmlab-hv watches its renderer's host GPU every 250 ms, on a thread that
holds no device lock (`hv/crates/vmlab-hv-devices/src/virtio/gpu/lost.rs`
at the root of the vmlab repository). It sees two losses:

- The render server ends. virglrenderer forks it while the renderer starts
  and does not run a new program in it, so the watch knows it as the child
  process that the start made. Nobody reaps it while the renderer lives, so
  an ended server is a zombie, or gone.
- The render node goes: the kernel no longer has its character device
  under `/sys/dev/char`, as after its driver is unbound or the GPU is
  removed. sysfs is read, not `/dev`, so the answer does not wait for udev.

On a loss the device:

- answers every fence it holds, so that no guest wait hangs;
- answers every later command that needs the host GPU `ERR_UNSPEC` at once,
  without a wait, and still answers the releases (`RESOURCE_UNREF`,
  `DETACH_BACKING`, `CTX_DESTROY`, `CTX_DETACH_RESOURCE`,
  `RESOURCE_UNMAP_BLOB`) `OK_NODATA`, after it has removed their host
  mappings from guest memory. The display info and the EDID are served as
  before;
- for a driver that acked the feature bit of C10,
  `VIRTIO_GPU_F_VMLAB_ORDERED_FLIP` (23), sets
  `VIRTIO_GPU_EVENT_VMLAB_DEVICE_LOST` (bit 31 of `events_read`) and raises
  a configuration interrupt. The driver clears it through `events_clear` as
  it clears the display event. A host GPU does not come back, so every later
  negotiation of the bit, after a device reset, sets it again;
- logs one error that names the cause, and keeps the VM running. The
  console shows a black screen from then on.

A driver without the bit (Linux, the drivers of a QEMU host) gets no event,
only the error answers. A restart of the VM starts a new renderer.

## The driver

`patches/kmd/0011` reads the event on the configuration interrupt and at
each negotiation, because no interrupt reaches a driver before
`DRIVER_OK`. The adapter is then lost until the next boot:

- Every 3D device is context-lost, as after a timeout reset. Its UMD gets
  `STATUS_DEVICE_REMOVED` from the next render, present or escape, and an
  application sees `DXGI_ERROR_DEVICE_REMOVED` or `VK_ERROR_DEVICE_LOST`.
- A new 3D context is refused with `STATUS_DEVICE_REMOVED`, so that an
  application can fall back to WARP.
- A DMA buffer of a GDI device retires at once, without the host. The host
  would answer it with errors, which the driver keeps for the timeout path,
  and Windows would reset the adapter again and again and then stop.
- A restart after a timeout reset restores no host objects. It would fail
  on them, and a failed restart stops Windows.

Event 10 of the driver's small trace ring records the loss.

## What is tested

- vmlab-hv's device tests, in the merge bar: the event only for a driver
  that acked the bit, once, cleared by the driver and raised again after a
  reset; the refused commands, the served display info and EDID, and a
  release that still releases; every held fence answered on the used ring
  with one queue interrupt; the stat parser and both probes against a fake
  `/proc` and `/sys/dev/char`.
- `a_render_server_that_ends_is_a_lost_host_gpu`, outside the merge bar
  (it needs `VMLAB_HV_TEST_RENDER_NODE`): a Venus renderer on a real GPU,
  whose render server the test kills. The watch names it within 5 s, and
  then the renderer refuses work at once, a `CTX_DESTROY` succeeds, and the
  reset and the drop end. It passed on helios on the Radeon (renderD130,
  AMD1) and the second RTX 5090 (renderD129, Nvidia2). It runs in a process
  of its own, because virglrenderer does not start again in a process whose
  render server was killed.

Not tested:

- The driver patch. `build/kmd/build.sh` applies it on `0001` to `0010`
  and builds version `100.6.101.58112` with no new compiler warning, but no
  Windows guest ran it. The test that it still
  needs: boot a `#win3d` clone on vmlab-hv with the driver, run a Direct3D
  11 probe, kill the VM's render server on the host, and check that the
  probe reports `DXGI_ERROR_DEVICE_REMOVED`, the desktop goes on with WARP,
  no bug check follows, and the VM answers its agent.
- A loss of real hardware. No GPU was made to fall off its bus. On such a
  loss the render node can stay, and the render server can live on with a
  Vulkan device that answers `VK_ERROR_DEVICE_LOST`; vmlab-hv does not see
  that yet. A test on real hardware needs a GPU that can be lost on
  purpose, a host that can lose it (not helios, whose GPUs other work
  uses), and a look at what the render server and the render node do then.
