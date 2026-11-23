# Review: the display path (viewer, VNC and RFB, hvdisplay)

Card #595, 2026-11-23. This is a fault list. It changes no code. The lead
makes the fix cards from it, the worst finding first.

Line numbers are those of commit `9dc9e64d` (`next` at the start of the
card). Short names used below:

- `display/` is `hv/crates/vmlab-hv-display/src/`
- `viewer/` is `hv/crates/vmlab-hv-viewer/src/`
- `vmm/display.rs` is `hv/crates/vmlab-hv-vmm/src/display.rs`

## How the review was done

The review is by reading. No test was written or run for a finding, so each
finding is a claim from the code, and the proposed test is what proves or
refutes it. No proposed test uses a wall-clock limit.

Severity words are those of the card: data loss, crash, security against a
hostile guest or peer, wrong result, cosmetic. "Confidence" says how sure the
reading is.

## Who can reach what

Three facts decide how bad most findings are.

- Under vmlab, `display.sock` and `vnc.sock` are in the machine runtime
  directory, below the lab runtime directory. `ensure_private_dir`
  (`src/paths.rs:291-322`) makes the runtime base and the lab directory mode
  0700 and refuses a directory of another owner. So under vmlab a peer on a
  display socket is a process of the same user. That user can already drive
  the lab through the lab control socket. Findings that need such a peer are
  robustness faults, and security faults only where vmlab-hv runs outside
  this layout (F7).
- The vmlab side (`src/vnc.rs`, `src/hvdisplay.rs`) is a client. Its peer is
  the hypervisor process of one machine: QEMU's VNC server or vmlab-hv's
  display server. A guest that has taken over its hypervisor process is the
  hostile peer there. These clients run inside the lab daemon, which serves
  every machine of the lab.
- A guest alone controls the framebuffer size up to the device limit, the
  cursor shape, the keyboard LEDs and how fast frames change. The display
  server checks the frame geometry (`display/core.rs:83-118`) and both cursor
  sources check the cursor (`vmlab-hv-devices/src/vdisplay.rs:293-299`,
  `virtio/gpu/mod.rs:1103`). No finding below is reached through pixel data
  alone.

## What was read and what was not

Read in full, without the test modules:

- `src/vnc.rs`, `src/hvdisplay.rs`
- `display/protocol.rs`, `core.rs`, `transport.rs`, `input.rs`, `lease.rs`,
  `client.rs`, `lib.rs`, and `rfb.rs` (the keysym table was only skimmed)
- `viewer/lib.rs`, `connection.rs` (1-350), `clipboard.rs` (1-265),
  `shell.rs`

Read in part:

- `viewer/render.rs`: image creation (196-225), `pack_damage` (521-552),
  `upload` (951-986) and `present` (1062-1210). The Vulkan device setup, the
  dma-buf import, the swapchain and the pipeline were not read.
- `viewer/input.rs` (240-297: key forwarding, buttons, the viewport and the
  pointer translation)
- `vmm/display.rs` (1-710). It is outside the scope of the card. It was read
  because it binds the two sockets, runs the accept loops and is the input
  sink.
- `src/labd/display/mod.rs` (95-175), `src/labd/hypervisor/vmlab_hv/display.rs`
  (30-113), `src/record/core.rs` (230-285), `src/record/executor.rs`
  (280-335), `src/viewer.rs` (25-60), `src/paths.rs` (72-322)
- `hv/crates/vmlab-hv-display/PROTOCOL.md` (searched for timeouts, limits and
  permissions, and lines 258-292 read)
- The four `retire` sites of the GPU device
  (`vmlab-hv-devices/src/virtio/gpu/renderer.rs:476-481`, `578`, `910`,
  `scanout.rs:749`)

Not read:

- `display/trace.rs`, `viewer/grab.rs`, `viewer/main.rs`, `viewer/input.rs`
  1-239 (the scancode table and the release chord), the shaders
- every test module and `src/vnc/fake.rs`, `src/hvdisplay/fake.rs` (first 60
  lines only), apart from the names of tests
- `src/cli/console.rs` and the rest of `src/viewer.rs` (which external viewer
  is started and how)
- QEMU's VNC server, the `-vnc` argument and the QMP input path
- the lab daemon side of `machine.clipboard_get` and `machine.clipboard_set`,
  and the agent's clipboard code beyond one check of `wl-paste --no-newline`
- the GPU device, apart from the `retire` sites. So what `needs_cpu()` costs
  per frame was not measured (N5).
- the Windows and Wayland viewer tests and the Python RFB client

## Findings, worst first

### F1. hvdisplay: a surface configure is used without a size limit, and two messages abort the lab daemon

- Where: `src/hvdisplay.rs:77-106` (`Surface::configure`), `109-126`
  (`Surface::read`), `172`, `182-188` (`rgb_from_bgrx`), `274`, `304-306`
  (`clamp16`), `350-362`.
- Fault: `configure` checks the kind, the format, `stride >= width * 4` and
  that a descriptor came. It does not limit `width`, `height` or
  `stride * height`, it accepts zero, and `width * 4` is a `u32`
  multiplication that wraps in a release build and panics in a debug build.
  vmlab-hv's own viewer checks all of this (`viewer/lib.rs:242-267`: 1 to
  16384, 256 MiB). `read` then allocates `w * 4 * h` bytes before it reads
  anything from the buffer. A configure of 1048576 x 1048576 with stride
  4194304, followed by one `FrameReady`, asks for 4 TiB. The allocation fails
  and Rust aborts the process: this is not a panic that `spawn_blocking`
  catches. The process is the lab daemon. Smaller wrong values give other
  results: a width above 65535 is cut to 65535 by `clamp16` for the
  `DesktopSize` and `Rect` events while `bgrx` keeps the real row length, so
  the recorder paints sheared rows. A width of `0x4000_0001` passes the
  wrapped stride check and ends in the `expect` of `rgb_from_bgrx`.
- Reach: the peer on `display.sock`, which is the vmlab-hv process of one
  machine. A guest that controls that process reaches the lab daemon of the
  whole lab. The server core never sends such a configure by itself
  (`display/core.rs:83-118`).
- Severity: crash of the lab daemon, security against a hostile peer.
- Proof: unit test `configure_refuses_a_surface_larger_than_the_protocol_limit`
  in `src/hvdisplay/tests.rs`. Build a `Message::SurfaceConfigure` with
  width 1048576, height 1048576, stride 4194304, kind `Shm`, one buffer, and
  pass it to `Surface::configure` with one memfd. Assert an error. Today it
  returns `Ok`. Add the cases width 0, width 70000 (stride 280000), and
  width `0x4000_0001` with stride 4. The last one panics today in a test
  build. A second test at the socket: a raw server made with
  `vmlab_hv_display::{receive, send}` answers the Hello, sends the configure
  with width 70000, height 1 and one 280000-byte memfd, then a `FrameReady`
  with full damage. Assert that `NativeStream::next_update` returns an error.
  Today it returns a `Rect` with `w = 65535` and 280000 bytes.
- Confidence: high.

### F2. vnc: rectangle, cursor and text sizes from the server are allocated before any check

- Where: `src/vnc.rs:100-104` and `127-129` (`reason_len`, `name_len`),
  `230-239`, `267-343` (`read_stream_rect`), `425-444` (`read_rect`),
  `447-451` (`skip`), `493-503` (`Canvas::new`).
- Fault: every length that the server sends is turned into a zeroed
  allocation at once. A Raw rectangle or a cursor allocates `w * h * 4` with
  16-bit `w` and `h`, up to 17 GiB from a 12-byte header. `read_rect` checks
  the rectangle against the framebuffer only in `Canvas::paint`, after the
  payload was read. `read_stream_rect` never checks it against
  `self.width` and `self.height`. `DesktopSize` 65535 x 65535 makes
  `Canvas::new` allocate 17 GiB too, and the recorder allocates 13 GiB for
  the same event (`src/record/core.rs:242`). `skip` allocates the whole
  length of a `ServerCutText` (32 bits) or of a refusal reason instead of
  reading through a fixed buffer. `next_update` keeps all rectangles of one
  update in memory until the update ends, and an update has up to 65535
  rectangles. An allocation that the host cannot reserve aborts the lab
  daemon. Where it can be reserved, memory grows with what the server sends.
- Reach: the peer on `vnc.sock`, which is the QEMU process of one machine.
  A guest reaches these values only through that process: QEMU itself sends
  rectangles inside the framebuffer.
- Severity: crash of the lab daemon on a host that cannot reserve the
  allocation, security against a hostile peer.
- Proof: test `stream_rejects_a_rectangle_outside_the_screen` in
  `src/vnc/tests.rs`, beside
  `capture_rejects_a_rectangle_outside_the_framebuffer`. The fake server
  announces 4 x 4, sends one update with one Raw rectangle 65535 x 65535 and
  16 bytes of payload, and closes. Assert that the error of `next_update`
  contains `outside`. Today the error is an unexpected end of file, after the
  17 GiB allocation. The same server against `capture()` must give `outside`
  too: today `read_rect` allocates and reads first. A third case sends a
  `ServerCutText` header with length `0xffff_ffff` and closes: assert an
  error that names the length.
- Confidence: high for the code. The abort depends on the memory and the
  overcommit setting of the host.

### F3. Display server: one failed accept ends both display sockets for the life of the VM, and nothing limits connections or the time of a handshake

- Where: `vmm/display.rs:673-699` (accept loop), `display/core.rs:380-393`
  (`serve`), `394-402` and `display/rfb.rs:80-92` (handshake reads with no
  timeout), `display/core.rs:652-654` (three memfds for each client with
  frames).
- Fault: the accept loop treats every error that is not `WouldBlock` as
  final: it logs, leaves the loop and calls `server.shutdown()`. `EMFILE`,
  `ENFILE` and `ECONNABORTED` are such errors, and they are temporary. After
  that the VM has no screen, no screenshot, no recording and no scripted
  input until it is started again. `std::thread::spawn` panics when the
  thread cannot be made, which ends the accept thread without the shutdown.
  The server starts one thread for each connection and waits for the first
  message with no timeout, so a connection that sends nothing holds a thread
  and two descriptors for ever. A client with frames costs three memfds of
  `stride * height` (up to 256 MiB each) and a full copy of each frame.
  `PROTOCOL.md:284-286` says that the caller controls permissions, socket
  timeouts and worker lifetime. The one caller sets none of them.
- Reach: a process of the same user, or a short descriptor shortage in the
  hypervisor at the moment a client connects. `vmlab-hv/src/main.rs:112-139`
  says that a guest with 3D programs can hold thousands of host descriptors.
- Severity: wrong result (the console is lost until the VM restarts). Crash
  where the thread limit is reached.
- Proof: test `display_survives_descriptor_exhaustion` at seam S1. Start
  vmlab-hv with a VGA machine and a low descriptor limit
  (`prlimit --nofile`). Open silent connections to `display.sock` until one
  is closed by the server or refused. Close all of them. Connect a client,
  send Hello and assert a Hello reply. Today the reply never comes: the
  listener is gone. Second test `unfinished_handshakes_are_bounded` in
  `hv/crates/vmlab-hv-display/tests/native.rs`: open 64 connections that
  send nothing and assert that the number of live sessions
  (`Shared::sessions`, or the entries of `/proc/self/task`) stays below a
  fixed limit. Today it grows by one for each connection.
- Confidence: high.

### F4. Display server: a dma-buf client that does not send Release stops the guest's GPU commands and a machine reset

- Where: `display/core.rs:539`, `612-616`, `674-676` (the held frame keeps a
  `BufferGuard`), `display/lease.rs:28-34` (`retire` waits with no limit),
  `vmlab-hv-devices/src/virtio/gpu/renderer.rs:476-481`, `570-581`,
  `908-911`.
- Fault: a client that negotiated `DMABUF_OK` holds a lease on the guest's
  buffer from `FrameReady` until its `Release`. The GPU device calls
  `lease.retire()` before several guest commands and in `reset()`, and
  `retire` waits until every reader has released. Nothing ends the wait: no
  deadline, no revocation, no disconnect of the client. A viewer that is
  stopped (`SIGSTOP`), hangs in its compositor, or is hostile keeps up to
  three leases. Then the guest's rendering stops and a reset of the machine
  waits in `reset()`. `PROTOCOL.md:73` says that holding a direct buffer can
  stall guest rendering, so the stall is known. That it has no upper limit
  and also holds a reset is the fault. The device side was a note in
  `docs/reviews/virtio-devices.md` (N13).
- Reach: a process of the same user on `display.sock`, or the user's own
  viewer when it stops.
- Severity: wrong result (the guest display and a reset hang for as long as
  the client stays connected).
- Proof: test `an_unreleased_dma_frame_cannot_hold_its_lease_for_ever` in
  `hv/crates/vmlab-hv-display/tests/native.rs`, with the leased source of
  `stable_dma_pool_keeps_imports_damage_and_release_tokens`. A client with
  `DMABUF_OK` receives the first `FrameReady` and sends no `Release`. The
  producer publishes three more frames on other buffers. Assert that
  `try_retire()` of the first lease is true, or that the server closed the
  client. Today it stays false until the client disconnects.
- Confidence: high for the display crate. The GPU device was read at the
  `retire` sites only.

### F5. Display server: CpuCopy is accepted from a client that never got a surface, and each one forces a capture and a full frame for every client

- Where: `display/core.rs:473-492`, `779-786` (`ClientState::surface`
  starts at 0), `738-742`.
- Fault: `CpuCopy` needs only the `CPU_COPY` capability. The check
  `s.surface != surface_id` passes for `surface_id = 0` on a connection that
  has no frames capability or has not received a configure yet. Each
  accepted `CpuCopy` runs `capture(true)`: one `source.frame()`, a forced
  new sequence number, and then every shared-memory client copies and writes
  the whole frame again and every RFB client sends a whole update. For a GPU
  scanout each copy is a readback. There is no rate limit, so 20 bytes on
  the socket cost a frame copy for each connected client. The RFB side has
  the same cost with a non-incremental request (`display/rfb.rs:213`).
- Reach: a process of the same user on `display.sock`.
- Severity: wrong result (a message that the protocol should refuse is
  served), and load on the hypervisor.
- Proof: test `cpu_copy_without_a_configured_surface_is_refused` in
  `hv/crates/vmlab-hv-display/tests/native.rs`. Connect with capabilities
  `CPU_COPY` only. Send `CpuCopy { surface_id: 0 }`. Assert that the server
  closes the connection and that the counting source saw no new `frame()`
  call. Today the connection stays open and the count rises by one for each
  message.
- Confidence: high.

### F6. Input: one pointer report that the guest never takes blocks every later key

- Where: `vmm/display.rs:538-575` (the retry loop), `display/input.rs:24-51`
  (one queue of 256 for keys and pointer), `display/core.rs:180-186` (one
  worker), `vmlab-hv-devices/src/usb/tablet.rs:247-270` (queue of 1024,
  `WouldBlock` when full).
- Fault: keys and pointer events share one queue and one worker. When the
  tablet's report queue is full, the sink retries the same event every
  millisecond until the guest reads a report or the machine exits. A guest
  that never polls the tablet (a boot loader, DOS, a paused machine, a guest
  without the driver) never drains it. Motion is merged, but each wheel step
  and each button change adds a report, so 1024 of them fill the queue.
  After that no key reaches the PS/2 keyboard, the shared queue fills, and
  the reader thread of every client blocks in `push`. A blocked reader no
  longer handles `Release`, so that client's frames stop after three.
  Scripted input through `InputConn` shares the queue: its keys are lost
  without an error. `PROTOCOL.md:278-282` describes the retry. The loss of
  the keyboard is not described.
- Reach: an ordinary user who scrolls or clicks in the viewer on such a
  guest, or a script that clicks before the guest has a tablet driver.
- Severity: wrong result (keyboard input is lost and the viewer freezes).
- Proof: test `a_full_tablet_queue_does_not_block_keys` in
  `hv/crates/vmlab-hv-vmm/src/display.rs`. Build the `Input` sink on a
  machine with a PS/2 keyboard and a USB tablet that no guest polls. Send
  1025 `Scroll` messages and then `Key { qcode: "a", down: true }` through a
  `DisplayServer`. Assert that the PS/2 device holds the scancode of `a`
  once the queue is drained up to the key. Today the worker never leaves the
  retry loop of the last scroll, so the key stays in the queue.
- Confidence: medium. The sink and the tablet queue were read. The xHCI and
  UHCI wrappers around `inject_pointer` were not.

### F7. The display sockets get no mode and no peer check from vmlab-hv

- Where: `vmm/display.rs:595-613` (`UnixListener::bind` twice),
  `display/core.rs:379` ("The caller owns socket binding, permissions"),
  `hv/crates/vmlab-hv-proto/src/machine.rs:471-475` (the paths are free).
- Fault: the sockets are bound with the mode that the umask leaves, and no
  connection is checked with `SO_PEERCRED`. Neither protocol has
  authentication (`display/rfb.rs:87-91` offers security type None only).
  A client that connects types keys, moves the pointer, resizes the display
  and reads the screen. Under vmlab the 0700 runtime directory protects the
  sockets. The machine description lets a caller put them anywhere, and
  vmlab-hv run by hand with a permissive umask, or in a shared directory,
  gives every local user the console of the guest.
- Reach: another local user, only outside vmlab's runtime layout.
- Severity: security against a local peer, limited to that case.
- Proof: test `display_sockets_are_owner_only` in
  `hv/crates/vmlab-hv-vmm/src/kvm_tests.rs`, beside the test that binds both
  sockets (line 78-90). Set the umask to 0, call `display::serve`, and assert
  that the mode of `display.sock` and `vnc.sock` is 0600. Today it is 0777.
  The test must restore the umask and must not run in parallel with tests
  that create files.
- Confidence: high.

## Notes

These have no proving test, were not confirmed, or are decisions that are
written down.

- N1. Clipboard: when the viewer window gets the focus, the host's text
  clipboard is sent to the guest without a paste (`viewer/clipboard.rs:109-115`,
  `164-168`), and the guest's clipboard replaces the host's once a second
  while the window has the focus (`177-187`). `viewer/README.md:81` documents
  it, and `src/viewer.rs:36-46` always passes `--lab-socket`, so it is always
  on. A hostile guest reads whatever the user copied last, a password for
  example. There is no switch to turn it off and no one-way mode.
- N2. Clipboard: `synchronize` sets the guest's text and reads it back in the
  same pass (`viewer/clipboard.rs:164-187`). If the agent returns a text that
  differs from what was set (line endings, a slow set), the host clipboard is
  overwritten with the guest's version. The Linux agent uses
  `wl-paste --no-newline`. The Windows and macOS paths were not read.
- N3. Viewer: a render error that is not an `io::Error` ends the viewer
  (`viewer/shell.rs:42-53`, `168-180`). `Image::new` is called with the
  guest's surface size (`viewer/render.rs:1090-1097`) and was not seen to
  check the device's `maxImageDimension2D`. A guest mode wider than the host
  GPU's limit may close the viewer. Not confirmed: it needs a GPU with a
  limit below 16384.
- N4. RFB server: a size change disconnects a client that did not ask for
  `DesktopSize` (`display/rfb.rs:235-238`). `VncClient::capture` asks for Raw
  only (`src/vnc.rs:373-376`), so it would be dropped. vmlab uses the native
  protocol for vmlab-hv, so only third-party viewers are affected.
- N5. RFB server: `serve_rfb` counts the connection as a CPU client before
  the handshake (`display/rfb.rs:82`). A connection to `vnc.sock` that sends
  nothing keeps `needs_cpu()` true, and the GPU device then makes a CPU copy
  of each frame. The cost was not measured.
- N6. Cursor: the cursor sequence number changes on every move of the guest
  pointer, because the position is part of `Cursor` (`display/core.rs:312-331`).
  A native client then gets the whole shape again for each move
  (`556-577`), and an RFB client gets the whole requested rectangle for an
  incremental request even when no pixel changed (`display/rfb.rs:210-216`,
  `250-275`). Both loops also clone the shape on every pass (`core.rs:546`,
  `rfb.rs:200`). This is load, not a wrong picture.
- N7. `native_frames` retries at once, with no wait, when the CPU readback
  returns `WouldBlock` (`display/core.rs:710-719`). The loop turns at full
  speed until the readback works. The other retry in the same function waits
  1 ms (`617-620`).
- N8. `VncClient::connect` has no timeout (`src/vnc.rs:89-136`), and
  `Display::vnc` opens a new connection for each operation
  (`src/labd/display/mod.rs:116-118`). A QEMU that accepts and does not
  answer holds a scripted input call for ever. Whether a caller above adds a
  timeout was not checked.
- N9. `NativeStream::connect` sets the 10 s read timeout for each message,
  not for the whole wait (`src/hvdisplay.rs:259-269`). A server that sends
  cursor messages and no configure keeps the call open. `capture_blocking`
  uses one deadline and is right (`139-149`).
- N10. `hvdisplay` reads pixels with `pread` from a descriptor that the peer
  chose (`src/hvdisplay.rs:117-123`). A descriptor of a file system that
  blocks (FUSE) holds a blocking-pool thread with no deadline. The capture
  deadline covers the socket only.
- N11. `InputConn::send` sends the whole batch again after a reconnect
  (`src/hvdisplay.rs:215-234`). If the first connection failed after part of
  the batch was delivered, those keys arrive twice. A restart of the
  hypervisor, the usual cause, delivers nothing on the old connection.
- N12. `InputConn` never reads (`src/hvdisplay.rs:200-203`). The server sends
  a `LedState` for each LED change, so after a few hundred changes the
  socket is full and that client's frames thread on the server stays in
  `send`. It holds no lock there and input still flows, so no effect was
  found.
- N13. Recorder, outside the scope: `src/record/core.rs:269` computes
  `(row * w + col) * 4` in `u32`. It wraps for a rectangle of more than 2^30
  pixels. The bounds check on line 270 keeps it from reading outside the
  buffer.
- N14. RFB server: a keysym that is not in the US table is dropped without a
  message (`display/rfb.rs:150-155`, `336-445`). A viewer with another layout
  loses those keys.
- N15. Viewer: packets go to the event loop through an unbounded queue
  (`viewer/shell.rs:480-482`). Frames are limited by the three held buffers.
  Cursor shapes are not, so a guest that changes its cursor faster than the
  viewer draws makes the queue grow. Not measured.

## Other work seen

- `PROTOCOL.md:284-286` gives permissions and socket timeouts to the caller,
  and the only caller sets none. The text or the caller should change with
  F3 and F7.
