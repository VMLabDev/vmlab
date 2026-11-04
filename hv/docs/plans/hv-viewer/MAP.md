---
slug: hv-viewer
status: specified
created: 2026-11-02
---

# A native, high-performance viewer for vmlab-hv machines

## Destination
`vmlab console` and `gui = true` open a native Rust viewer window for vmlab-hv machines; QEMU machines keep their external VNC viewers. GPU guests (virgl/Venus) reach the window by zero-copy dma-buf import, 2D guests by shared memory with only damaged regions uploaded. The viewer keeps up with the guest up to the display's refresh rate and adds no more than one frame of latency to display and input. Keyboard and absolute pointer work with a grab and a release chord, the guest resolution follows the window size, and clipboard works through the vmlab agent. Linux Wayland and X11, with a CPU-copy fallback on WSL2. Audio is not part of the viewer: vmlab-hv already plays guest audio to the host's PipeWire/PulseAudio per machine. Delivered as the full destination in one build (owner, Q95=2, 2026-11-02).

## Grounding
notes/01-grounding.md (research). Key facts:
- Native display protocol already carries input (Key by QEMU key name, PointerAbs, Buttons, Scroll, LedState) and RequestSize (virtio-gpu display info + EDID); std VGA ignores RequestSize; no horizontal scroll, no relative pointer (crates/vmlab-hv-display/PROTOCOL.md:135-165; virtio/gpu/mod.rs:203-217, 372-391).
- 3D frames are NOT zero-copy today: the hypervisor EGL-imports the scanout, blits into a fresh XR24 buffer (the NVIDIA external-only workaround), sends a new surface/fd per frame with full damage, also reads every frame back to CPU, and publishes on a fixed 16 ms poll (scanout.rs:312-373, 435-445; vm.rs:1125-1135).
- Existing display `Client` does handshake and blocking send/receive only; multiple native clients may connect at once.
- Agent clipboard is text-only request/reply, no change notification; reachable via labd machine.clipboard_get/set.
- vmlab console.rs and viewer::open_for always use vnc.sock and do not know the machine's hypervisor; status has no hypervisor field; display socket is machine_runtime_dir(...)/display.sock.
- No GUI/windowing crate in either workspace; no prior native viewer attempt.
- PipeWire/Pulse audio backends are cargo features off by default in vmlab-hv (crates/vmlab-hv/Cargo.toml:21-24).
- Binding: MIT licence; QEMU source readable as reference, never copied (AGENTS.md).

## Decisions
| ID | Decision | Struck by | Recorded in |
|----|----------|-----------|-------------|
| D1 | winit for windows and input, Vulkan through `ash` for rendering, importing guest dma-bufs with VK_EXT_external_memory_dma_buf and VK_EXT_image_drm_format_modifier. | | `notes/02-windowing-and-graphics-stack.md` |
| D2 | Yes, in this work: stable surfaces with cached buffer imports on both sides; send the guest scanout dma-buf directly and blit only where an NVIDIA external-only buffer requires it; no CPU readback unless a client asked for shared-memory frames; damage and publish driven by guest resource flushes instead of the fixed 16 ms poll. | | `notes/03-hypervisor-frame-export-rework.md` |
| D3 | Map evdev keycodes to the protocol's QEMU key names. | | `notes/04-keyboard-grab-and-release-chord.md` |
| D4 | Send RequestSize after a 200 ms debounce while the window is dragged; std VGA guests, which ignore RequestSize, are scaled into the window with letterboxing. | | `notes/05-resize.md` |
| D5 | Through the lab daemon's machine. | | `notes/06-clipboard-route.md` |
| D6 | Detect WSLg (/mnt/wslg or a WAYLAND_DISPLAY pointing there) and use the shared-memory frame path with a CPU upload; everything else unchanged. | | `notes/07-wsl2.md` |
| D7 | A `vmlab-hv-viewer` binary in the vmlab-hypervisor workspace, installed beside vmlab-hv; vmlab finds it on PATH or through a host-config key, adds the machine's hypervisor to its status, and `vmlab console` and `gui = true` launch it for vmlab-hv machines (QEMU machines keep VNC viewers). | | `notes/08-how-vmlab-chooses-and-finds-the-viewer.md` |
| D8 | A headless viewer core (protocol frames in, input out) tested against a fake display server; an S1 test that drives a real guest and checks frames and input round-trip; a manual checklist for the window itself. | | `notes/09-test-seam.md` |
| D9 | Enable vmlab-hv's PipeWire and PulseAudio backends in default and release builds. | | `notes/10-audio-backends.md` |

## Frontier
- [x] Q95 (decision) — Milestone shape: full destination in one build (owner). Recorded in Destination.
- [x] Q96 (decision) — Windowing and graphics stack for display and dma-buf import. → D1
- [x] Q97 (decision) — Rework the hypervisor's frame export for zero-copy (stable surfaces, no blit/readback except where required, flush-driven damage). → D2
- [x] Q98 (decision) — Keyboard mapping, grab and release-chord policy. → D3
- [x] Q99 (decision) — Resize: debounce, and what std VGA guests get. → D4
- [x] Q100 (decision) — Clipboard route. → D5
- [x] Q101 (decision) — WSL2 detection and fallback path. → D6
- [x] Q102 (decision) — How vmlab chooses and finds the viewer. → D7
- [x] Q103 (decision) — Test seam for the viewer. → D8
- [x] Q104 (decision) — Enable vmlab-hv's PipeWire/Pulse audio backends in default builds. → D9

## Fog

## Accepted unknowns
- Shipping vmlab-hv-viewer in a release (D7): vmlab-hypervisor has no release job yet; manual install for now.
- Exact chord configuration surface (D3): a spec detail.

## Out of scope
- Viewing QEMU machines; multiple monitors; USB redirection through the viewer; viewing over the network; microphone input.
