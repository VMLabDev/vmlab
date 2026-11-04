# A native, high-performance viewer for vmlab-hv machines

## Problem

The only way to watch or drive a vmlab-hv machine's screen is an external VNC
client against `vnc.sock`. A GPU guest's frame is read back from the GPU,
re-encoded as RFB and decoded again, so virgl and Venus desktops are not smooth
to use by hand. vmlab-hv already has a native display protocol with
shared-memory and dma-buf frames, input and resize, but nothing consumes it
interactively. The hypervisor's export is also not zero-copy today:
- It blits every 3D frame into a fresh buffer.
- It reads every frame back to the CPU.
- It sends a new surface and file descriptor per frame, with full damage.
- It publishes on a fixed 16 ms poll.

## Solution

A `vmlab-hv-viewer` binary in the vmlab-hypervisor workspace, which vmlab
launches for vmlab-hv machines. It is made of:
- **Windowing:** winit (D1).
- **Rendering:** Vulkan through `ash`, importing guest dma-bufs with their DRM
  modifiers (D1).
- **Input:** keyboard and absolute pointer over the existing display protocol,
  with a configurable grab release chord (D3).
- **Resize:** follows the window through RequestSize (D4).
- **Clipboard:** through the lab daemon (D5).
- **WSL2:** a shared-memory CPU-upload path (D6).

The hypervisor's frame export is reworked for zero-copy (D2). vmlab learns each
machine's hypervisor and picks the viewer accordingly (D7). Guest audio is not
the viewer's job: vmlab-hv's PipeWire and PulseAudio backends become default
(D9).

## User stories

1. As a vmlab user, `vmlab console <vm>` on a vmlab-hv machine opens a native
   window showing the guest's desktop.
2. As a vmlab user, a virgl or Venus guest's desktop in that window keeps up
   with the guest's frame rate, up to the host display's refresh rate, with no
   more than one frame of added latency.
3. As a vmlab user, clicking the window grabs keyboard and pointer, and the
   release chord (left Ctrl+Alt by default) gives them back.
4. As a vmlab user, resizing the window resizes the guest's desktop. On a
   std-VGA guest the picture is scaled instead.
5. As a vmlab user, copying text on the host and pasting it in the guest works,
   and the other way round.
6. As a vmlab user on WSL2, the viewer works through the CPU-copy path.
7. As a vmlab user, guest audio plays on the host without building vmlab-hv
   with extra features.
8. As a vmlab user, QEMU machines keep opening their external VNC viewer.

## Implementation decisions

### Hypervisor: frame export rework (D2), `vmlab-hv-display` and `vmlab-hv-vmm`

- **Stable surfaces.** A guest scanout keeps one surface id across frames. A
  new buffer is announced only when the guest's scanout resource changes.
  Clients cache imported buffers by surface and buffer id.
- **Direct dma-buf.** Clients that negotiated dma-buf receive the guest scanout
  resource's dma-buf with its DRM format and modifier, without a blit.
  - **When it still blits:** only when the scanout is NVIDIA external-only or
    otherwise unexportable. That stays the existing EGL import-and-blit, into a
    small pool of reused buffers rather than a fresh one per frame.
- **No CPU readback unless asked.** A client that negotiated shared-memory
  frames gets them, and so do vmlab's recorder and screenshot consumers and the
  RFB server. No client needing CPU pixels means no readback.
- **Flush-driven publishing.** A frame is published on the guest's
  `RESOURCE_FLUSH` or `SET_SCANOUT`, carrying the flush's damage rectangles.
  The fixed 16 ms poll stays only for the 2D VGA paths that have no flush
  signal.
- **Fences.** A frame carries a fence or release token, so the guest does not
  reuse a buffer the viewer is still sampling. The protocol gains whatever
  explicit release the client needs, as an additive, versioned change.
- **Placeholder flags.** Placeholder-frame semantics from the recording work are
  preserved.

### Viewer binary: `vmlab-hv-viewer` (new crate)

- **Core (headless) and shell (window), split for testing (D8).**
  - **The core** owns the display-protocol client, the surface and buffer cache,
    damage tracking, input translation and resize debounce.
  - **The shell** owns winit, the Vulkan swapchain and presentation.
- **Command line.**
  - The display socket path.
  - The lab daemon socket and the machine identity, for clipboard.
  - The release chord.
  - `--shm`, which forces the shared-memory path.
- **Rendering (D1).**
  - **dma-buf frames:** imported as `VkImage`s with
    `VK_EXT_external_memory_dma_buf` and `VK_EXT_image_drm_format_modifier`,
    cached per buffer, and sampled into the swapchain image.
  - **Shared-memory frames:** only damaged rectangles are uploaded to a staging
    image.
  - **Pacing:** presentation uses FIFO, or MAILBOX where available, so latency
    stays within one frame.
  - **Cursor:** drawn from the protocol's cursor messages.
- **Input (D3).**
  - **Keys:** winit events with evdev scancodes, mapped to the protocol's QEMU
    key names.
  - **Pointer:** absolute positions scaled to the guest's resolution, with
    buttons and vertical scroll.
  - **LEDs:** LED state is mirrored.
  - **Grab:** click grabs, and the release chord (default left Ctrl+Alt,
    configurable) releases. The title shows the chord and the grab state.
- **Resize (D4).** RequestSize is sent after 200 ms with no further size change.
  A std-VGA guest, which ignores RequestSize, is scaled with letterboxing.
- **Clipboard (D5).**
  - **Transport:** the lab daemon's `machine.clipboard_get` and
    `machine.clipboard_set`. Text only.
  - **Host to guest:** on focus gain and on the host's selection change while
    focused.
  - **Guest to host:** polled once a second while the window has focus, and set
    on the host clipboard when it changes.
- **WSL2 (D6).** WSLg is detected by `/mnt/wslg`, or a `WAYLAND_DISPLAY` there,
  and the viewer then negotiates shared-memory frames only.

### vmlab (D7)

- Machine status gains the machine's hypervisor.
- `vmlab console` and `gui = true` launch `vmlab-hv-viewer` for vmlab-hv
  machines, passing the display socket, the lab daemon socket and the machine
  identity. QEMU machines keep the existing VNC viewers.
- `vmlab-hv-viewer` is found on PATH or through a host-config key, following the
  `vmlab_hv` binary pattern.
- vmlab's own native-frame consumers (the recorder and screenshots) keep
  requesting shared memory and keep working.

### Audio (D9)

- vmlab-hv's PipeWire and PulseAudio backend features are on in default and
  release builds. The README lists the build dependencies, libpipewire and
  libpulse.

## Testing decisions

- **Viewer core against a fake display server (D8):**
  - Surfaces and the buffer cache across frames.
  - Damage-limited shared-memory uploads.
  - Key mapping for the full keyboard.
  - Pointer scaling, and resize debounce.
  - Grab and release-chord state.
  - Clipboard polling against a fake lab daemon.
- **Hypervisor export (S1):**
  - **A virgl guest:** the export sends the scanout dma-buf without a blit and
    with stable surface ids, frames follow guest flushes with their damage, and
    no CPU readback happens while only a dma-buf client is connected.
  - **The recorder:** a shared-memory client receives correct frames
    concurrently.
  - **NVIDIA external-only scanouts** still work through the pooled blit.
- **End to end (S1):** a viewer core, without a window, driving a real guest
  sends keys and pointer input that the guest receives (checked through the
  agent), and gets a resize the guest applies.
- **Latency:** time from a guest flush to the frame being available to the
  client, measured on both the dma-buf and shared-memory paths and recorded.
  Display-side latency is checked manually.
- **Manual checklist:**
  - A GPU Linux desktop, a Windows desktop and a std-VGA guest.
  - Grab and release, resizing and clipboard.
  - X11 and Wayland, on NVIDIA and on the AMD iGPU.
  - WSL2, if available.
- **Merge bar:** `just ci::check`, with GPU and window tests skipping cleanly
  where there is no GPU or display.

## Out of scope

- Viewing QEMU machines.
- Multiple monitors.
- USB redirection through the viewer.
- Viewing over the network.
- Microphone input.
- Audio inside the viewer.
- A direct Wayland compositor path (`linux-dmabuf` subsurface).
- An agent clipboard change notification.
- Shipping the viewer in a release (vmlab-hypervisor has no release job yet).

## Open questions

- Whether every guest scanout resource rutabaga produces is exportable with a
  modifier the host's Vulkan can import, on both NVIDIA and the AMD iGPU. The
  pooled blit is the fallback, and the build measures how often it triggers.
- How Venus scanouts differ from virgl's in exportability, and whether flushes
  arrive the same way.
