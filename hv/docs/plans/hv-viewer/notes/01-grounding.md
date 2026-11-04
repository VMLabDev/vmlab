---
id: "01"
kind: research
---

# Grounding: what exists for a vmlab-hv viewer

Paths are relative to `vmlab-hypervisor/` unless prefixed `vmlab:`. Two INTAKE
premises are already stale: input and resize **are** on the display socket, and
guest audio **already** plays (sections 1, 3, 4).

## 1. The native display protocol (`crates/vmlab-hv-display`)

- Wire: AF_UNIX stream, 8-byte header (u32 length, u16 kind, u16 flags), LE, 16 MiB cap; fds ride SCM_RIGHTS on the first header byte (`crates/vmlab-hv-display/PROTOCOL.md:19-36`). Version 1, 13 kinds (`PROTOCOL.md:3`, `src/protocol.rs:6`).
- Capabilities: FRAMES=1, CURSOR=2, INPUT=4, DMABUF_OK=8, CPU_COPY=16, FRAME_FLAGS=32 (`src/protocol.rs:14-20`); server replies with the intersection (`PROTOCOL.md:43-52`).
- Surfaces: `SurfaceConfigure` names shm (kind 0) or dmabuf (kind 1), fourcc, stride, modifier, 1..16 fds; shm is three sealed, read-only-mappable memfds, linear, XRGB/ARGB8888, single plane (`PROTOCOL.md:54-69`).
- Frames: `FrameReady` = surface, buffer index, seq, timestamp, damage rects (`PROTOCOL.md:71-76`); shm pixels are complete even under partial damage (`PROTOCOL.md:94-95`); a dma-buf frame may carry one acquire sync_file (`PROTOCOL.md:97-100`). Header flag bit 0 = host placeholder ("Display output is not active.") under FRAME_FLAGS (`PROTOCOL.md:78-89`, `src/protocol.rs:30`).
- Flow control: client must `Release` each frame; at most three held per client; on release after skips the server sends latest with full damage (`PROTOCOL.md:102-107, 204-209`). Every dma-buf publication is a **new configure** (new fd) (`PROTOCOL.md:207-208`).
- Cursor: `CursorShape` premultiplied ARGB ≤1024² + hotspot, `CursorState` x/y/visible (`PROTOCOL.md:109-123`). Only virtio-vga reports one; std VGA paints its own (`crates/vmlab-hv-vmm/src/display.rs:115-133`).
- `CpuCopy` flips one connection to shm readback, others unaffected (`PROTOCOL.md:125-133`); a client without DMABUF_OK gets CPU frames directly (`src/core.rs:377`).
- **Input is on this socket**: `Key` (QEMU qcode string), `PointerAbs` (x,y relative to supplied w,h), `Buttons` (full mask), `Scroll` (wheel steps), `LedState` back (`PROTOCOL.md:135-165`). The VMM routes keys to the xHCI keyboard else PS/2, pointer to the xHCI tablet (`crates/vmlab-hv-vmm/src/display.rs:291-331`). No relative pointer; horizontal scroll `dx` is dropped (`display.rs:286`).
- `Client` is a thin blocking handshake + `receive`/`send`/`release`/`cpu_copy` with split read/write locks; no auto-release, no fence wait, no mmap (`src/client.rs:7-66`, `PROTOCOL.md:225-230`). Crate depends only on libc (`Cargo.toml:9-10`).
- Many native clients may connect at once, one worker each (`crates/vmlab-hv-vmm/src/display.rs:396-420`), so a viewer coexists with vmlab's recorder and screenshots (`vmlab:src/hvdisplay.rs:14-18`).
- RFB server on `vnc.sock` beside it; RFB clipboard is consumed and ignored (`PROTOCOL.md:238-263`).

## 2. GPU guests (virgl/Venus via rutabaga)

- rutabaga_gfx 0.1.80 with virgl_renderer (`crates/vmlab-hv-devices/Cargo.toml:27`). Scanout resource is exported as a dma-buf via `export_blob`, refusing non-dmabuf handles (`crates/vmlab-hv-devices/src/virtio/gpu/renderer.rs:313-332`).
- The hypervisor does **not** forward the guest's buffer. `Scanout::capture` EGL-imports it as an external texture (covers NVIDIA external-only and any modifier) and blits into a fresh `gbm_bo` XR24, exporting that fd and its gbm modifier (`virtio/gpu/scanout.rs:1-2, 241-310, 312-373`), **and** `glReadPixels` + `glFinish` into a CPU copy on every capture (`scanout.rs:435-445`). Acquire fence via EGL_ANDROID_native_fence_sync when available (`scanout.rs:467-479`).
- So a 3D frame = dmabuf configure, XR24, gbm modifier (possibly tiled), full-frame damage always (`crates/vmlab-hv-vmm/src/display.rs:183-213`); `readback` returns the pre-made CPU copy (`display.rs:250-260`). A 2D frame = shm with real damage from VGA dirty state and the KVM dirty log (`display.rs:215-248`, `PROTOCOL.md:200-202`).
- Publication is a 16 ms poll, not guest-flush driven (`crates/vmlab-hv-vmm/src/vm.rs:1125-1135`) — caps delivery near 60 Hz regardless of host refresh.
- Proto tests cover only synthetic dma-bufs, not real GPU import or NVIDIA (`PROTOCOL.md:276-279`).

## 3. Resolution changes

- `RequestSize {w,h}` (kind 13, needs INPUT, 1..16384) exists (`PROTOCOL.md:167-173`); the VMM forwards it to virtio-vga only (`crates/vmlab-hv-vmm/src/display.rs:134-138, 313-317`).
- virtio-gpu stores the preferred size, sets the DISPLAY config event and raises a config interrupt (`virtio/gpu/mod.rs:203-217`); `GET_DISPLAY_INFO` reports it (`mod.rs:372-378`) and `GET_EDID` returns a captured EDID with the preferred detailed timing rewritten (`mod.rs:379-391, 869-892`). The guest chooses when to apply (`PROTOCOL.md:171-172`).
- Standard VGA keeps its guest-programmed mode (`PROTOCOL.md:172-173`); its EDID is fixed at 1280x800 (`crates/vmlab-hv-devices/src/display/mod.rs:973`). An acceptance test drives RequestSize (`crates/vmlab-hv/src/display_acceptance.rs:446`).

## 4. Audio

- Built: machine description `audio` = `hda | virtio_snd`, microphone flag, PipeWire node name (`crates/vmlab-hv-proto/src/machine.rs:82-84, 151-174`); backend order PipeWire, else `PULSE_SERVER`, else null (`crates/vmlab-hv-vmm/src/audio.rs:1-8`, `crates/vmlab-hv-devices/src/audio/mod.rs:1-12`); virtio-snd wired in the builder (`crates/vmlab-hv-vmm/src/builder.rs:258, 1193-1198`).
- vmlab emits HDA at 00:1b.0 for `audio = true` on every guest; virtio-snd waits on its pinned rev (`vmlab:src/labd/hypervisor/vmlab_hv/describe.rs:208-221`). PRD §11 documents it, WSLg via `PULSE_SERVER` included (`vmlab:docs/vmlab-prd.md:901`), phase 2e (`vmlab:docs/vmlab-prd.md:2591`).
- Caveat: PipeWire/Pulse backends are cargo features; a default `vmlab-hv` build has only the null sink (`crates/vmlab-hv/Cargo.toml:21-24`). The INTAKE's audio constraint and PRD-change item are obsolete; the viewer need not carry audio.

## 5. Clipboard

- Agent vocabulary: `SetClipboard`/`GetClipboard` request-reply, text only, `Clipboard{text}`/`ClipboardSet`/`ClipboardFailed` (`vmlab:guest/agent-proto/src/lib.rs:69-81, 502-509, 634`). No change notification exists, so the host can only poll.
- Linux uses `wl-copy`/`xclip` when a display is reachable (`vmlab:guest/agent/src/unix/linux.rs:282-310`); Windows spawns a helper into the desktop session and refuses with nobody logged on (`vmlab:guest/agent/src/clipboard.rs:9-25`); macOS uses the console pasteboard (`vmlab:docs/vmlab-prd.md:547`).
- The agent socket is held by the lab daemon (`vmlab:src/labd/vm_agent.rs`); GUI-reachable surface today is only the lab-daemon verbs `machine.clipboard_get/set` (`vmlab:src/proto/vocab.rs:846-854`, `vmlab:src/labd/mod.rs:1019-1052`) and the `vmlab clipboard get|set` CLI (`vmlab:src/cli/machine.rs:195-239`). Nothing in either repo bridges it to a host clipboard.

## 6. How vmlab opens a viewer today

- `vmlab console` resolves `<runtime>/<lab>/vms/<vm>/vnc.sock` unconditionally and runs `viewer::detect()` (host config `viewer`, else remote-viewer/gvncviewer/vncviewer on PATH, all TCP-bridged via detached `vmlab __vncbridge`); `--tcp` or no viewer prints a bridge address (WSL2 path) (`vmlab:src/cli/console.rs:18-71`, `vmlab:src/viewer.rs:52-70, 131-153`).
- `gui = true`: `vmlab up` calls `viewer::open_for` per gui VM from the CLI, not the daemon (`vmlab:src/cli/lab.rs:322-336`, `vmlab:src/viewer.rs:156-181`); template builds call `open_when_ready(vnc_sock)` (`vmlab:src/template/build.rs:560-561, 1135-1136`). PRD §11 (`vmlab:docs/vmlab-prd.md:893-899`).
- Neither path knows the hypervisor. The CLI could resolve it with `HypervisorKind::effective` (`vmlab:src/config/hypervisor.rs:59`); the status protocol carries no hypervisor field (no match in `vmlab:src/proto/`), though `status -v` prints one (`vmlab:docs/vmlab-prd.md:2579`).
- Display socket: `run.join("display.sock")` beside `control.sock` (`vmlab:src/labd/vm.rs:179-197`), written into the description (`vmlab:src/labd/hypervisor/vmlab_hv/describe.rs:555-556`), i.e. `machine_runtime_dir(lab, Vm, vm)/display.sock` (`vmlab:src/paths.rs:153-166`). vmlab already consumes it via `vmlab-hv-display` pinned by git rev (`vmlab:Cargo.toml:47-49`, `vmlab:src/hvdisplay.rs:1-18`), requesting FRAMES|CURSOR|FRAME_FLAGS or INPUT, never DMABUF_OK (`vmlab:src/hvdisplay.rs:138, 218, 257`).
- Binary discovery precedent: host config `vmlab_hv`, else `vmlab-hv` on PATH (`vmlab:src/config/host.rs:85-86`, `vmlab:src/labd/hypervisor/vmlab_hv/mod.rs:35-37`).

## 7. GUI/graphics dependencies and prior attempts

- vmlab `Cargo.lock`: none of winit, wgpu, ash, egui/eframe, smithay-client-toolkit, glutin, sdl2, softbuffer, glow, wayland-client, x11rb, arboard.
- vmlab-hypervisor: no windowing crate. Graphics present is headless only: `glow` 0.16 plus hand-linked libEGL/libgbm FFI in the device crate (`crates/vmlab-hv-devices/Cargo.toml:30`, `virtio/gpu/scanout.rs:21-58`); `pipewire` 0.10.1 and `libpulse-binding` optional (`crates/vmlab-hv-devices/Cargo.toml:13-14`).
- History: no viewer/winit/wgpu/egui commit or branch in vmlab-hypervisor. vmlab once used QEMU's own GTK window for `gui = true` (`c5a9fd5`), replaced by external VNC viewers (`38a69c7`, `558c53a`); the only in-tree GUI ever was the SolidJS web console (`9c34114`), removed (`aad8912`). No native Rust viewer has been tried.

## 8. Constraints

- MIT workspace licence; new crates under `crates/*` join automatically (`Cargo.toml:3, 8`). Clean-room: QEMU source may be read, never copied or translated; crosvm lifts keep BSD-3 + THIRD_PARTY_NOTICES; Apache-2.0 crates as dependencies only (`AGENTS.md:33-39`).
- Done means `just ci::check` (fmt, clippy -D warnings, tests) and a test at a named seam (S1 control socket / S2 disk) (`AGENTS.md:45-47`); a viewer has no seam yet — the display socket would need naming. Spec at `.scratch/plans/vmlab-hypervisor/SPEC.md` is the contract (`AGENTS.md:8`).
- Shipping: the hv repo has CI only (`.github/workflows/ci.yml`), no release job; its justfile builds `vmlab-hv` only for bench (`justfile:144-147`). vmlab finds `vmlab-hv` by PATH or host config, so a `vmlab-hv-viewer` beside it on PATH (plus an optional host-config key) follows precedent. Wire changes need vmlab's pinned rev bumped (`vmlab:Cargo.toml:46-49`).

## Consequences

- Toolkit and graphics API: winit + raw Vulkan (ash, `VK_EXT_image_drm_format_modifier`/external memory) or EGL/GL (glow is already in-tree), or wgpu with a native-import escape hatch? Does it need any widget toolkit, or only a surface?
- Is dma-buf even worth it as built? The server already blits plus reads back every 3D frame and polls at 16 ms (§2). Should the plan include hypervisor work: skip readback when no CPU client, publish on guest flush, forward the guest buffer, real damage?
- Per-frame configure on dma-buf: should the viewer cache imports by fd/inode, or should the server pool a small ring of gbm bos?
- Input: the socket covers keys, absolute pointer, buttons, vertical wheel. Who maps host keycodes/keysyms to qcodes (RFB has a keysym table at `crates/vmlab-hv-display/src/rfb.rs`)? Is horizontal scroll or relative pointer needed? Grab and release chord policy?
- Resize: RequestSize already reaches virtio-gpu; debounce, and what on std VGA (scale/letterbox)? Is a guest-side agent path needed for guests whose compositor ignores the EDID hotplug?
- Clipboard: viewer -> lab daemon `machine.clipboard_get/set` (polling, needs vmlab's protocol), a new agent push notification, or a display-protocol extension the hypervisor proxies? Does the viewer then depend on vmlab's proto crate?
- WSL2: request no DMABUF_OK (shm path) on WSLg — detect how, and is the shm damage path fast enough with 3D frames always full-damage?
- Launch: how does the vmlab CLI decide QEMU vs vmlab-hv per machine (config resolution vs a status field), and is the binary located by PATH or a new host-config key? Template-build `gui` too?
- Audio: confirm out of viewer scope and drop it from the INTAKE; should vmlab's packaging ensure vmlab-hv is built with `pipewire`/`pulse`?
- Testing seam: what seam proves the viewer (headless render to an offscreen target against a fake DisplayServer)?
