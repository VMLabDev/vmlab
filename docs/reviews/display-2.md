# Second review: the display path (viewer, VNC and RFB, hvdisplay)

Card #1063, 2026-11-26. A second fault-finding pass over the display path. It
lists faults and makes no fix. Reviewed at `next` commit `0d756e5a`. All
paths are from the repository root, and all line numbers are from that
commit.

The first pass is `docs/reviews/display.md` (card #595, findings F1 to F7,
notes N1 to N15). All seven of its findings are marked fixed. This document
continues its numbering, so a code names one finding across both: F8 to F15
and N16 to N30 are new here. A finding of the first pass is named again only
where its fix is incomplete, and the text says which.

Short names used below, as in the first pass:

- `display/` is `hv/crates/vmlab-hv-display/src/`
- `viewer/` is `hv/crates/vmlab-hv-viewer/src/`
- `vmm/display.rs` is `hv/crates/vmlab-hv-vmm/src/display.rs`

Severity words are those of the first pass: **data loss**, **crash**,
**security**, **wrong result**, **cosmetic**.

## How the findings were checked

Every finding is from reading the code. No test was written or run for a
finding, and no vmlab code was run for this review. Each finding names the
test that would prove it. A finding whose test does not reproduce is wrong
and should be dropped. No test below uses a wall-clock limit. Three of the
tests (F10, F11, F12) wait for an event that never comes today, so today
they do not end. Their assertions use no clock: the test harness's own
timeout is what shows the failure.

The reach of each fault follows the three facts of the first pass ("Who can
reach what"). One of them has changed since then: QEMU runs no x86 guest any
more, so `src/vnc.rs` now talks to QEMU only for aarch64 and riscv64 guests,
and to vmlab-hv's own RFB server for a capture of a machine with a `gpu {}`
(`src/labd/display/mod.rs:123-126`).

## Findings, worst first

| Code | Severity | Fault in one line |
|------|----------|-------------------|
| F8 | crash, security | hvdisplay reads every damage rectangle of one `FrameReady` into memory with no limit on the count or the sum (the limit that F2's fix gave `vnc.rs` is missing here) |
| F9 | security | The console TCP bridge gives the unauthenticated console of a guest to every local user, past the socket mode and the peer check of F7's fix |
| F10 | wrong result | Keys and pointer buttons that a display client pressed stay down in the guest after that client disconnects |
| F11 | wrong result | hvdisplay's handshake has no timeout: a hypervisor that accepts and does not answer holds a capture for ever, and holds the machine's input lock for ever |
| F12 | wrong result | The console TCP bridge does not pass a close from one side to the other: a viewer stays on a frozen picture after a restart, and a closed viewer keeps its session in the display server |
| F13 | wrong result | The viewer replaces a host clipboard that holds no text (an image, files) with the guest's old text when its window gets the focus |
| F14 | wrong result | A VGA mode whose height computes to 0 makes every publish fail: one warning per 16 ms, and new display clients are refused |
| F15 | cosmetic | `gui = true` on a template build opens no viewer unless `host.viewer` is set, and its message tells the user to install a viewer that the code never uses there |

### F8. hvdisplay: the damage of one frame is read with no limit (crash, security)

- Where: `src/hvdisplay.rs:357-387` (`decode`, the `FrameReady` arm),
  `129-146` (`Surface::read`), `335-342` (`read_frames`),
  `display/protocol.rs:325-328` (the only limit on the damage count),
  `display/core.rs:119` (the server's own limit of 65535).
- Why it is a fault: `decode` loops over every rectangle of a `FrameReady`.
  It clips each one to the surface, and for each one `Surface::read`
  allocates `w * 4 * h` bytes and fills them from the shared buffer. All of
  them are pushed into one `Vec<StreamEvent>` before the batch goes to the
  recorder. The clip bounds one rectangle to one surface (256 MiB at most).
  Nothing bounds the number of rectangles or their sum. The codec accepts
  any count that fits the message: a 16 MiB message carries about 1,048,000
  rectangles. One such message, each rectangle the whole of an 8192 x 8192
  surface, asks the lab daemon to read 256 MiB a million times into memory
  that it keeps. The buffer is a memfd of the peer and needs 256 MiB only.
  The daemon is killed for memory, or aborts on the allocation, long before
  the batch ends. F2's fix gave `src/vnc.rs` exactly this limit
  (`MAX_UPDATE_BYTES`, `src/vnc.rs:87-91`, `spend` at `512-520`). The fix of
  F1 limited the surface in `hvdisplay` and not the damage. The server core
  refuses a frame with more than 65535 rectangles and with a rectangle
  outside the frame, so an honest vmlab-hv never sends this.
- Reach: the peer on `display.sock`, which is the vmlab-hv process of one
  machine, while a recording of that machine runs (`NativeStream`). A guest
  that controls that process reaches the lab daemon of the whole lab. This
  is the reach of F1.
- Proof: test `the_stream_refuses_more_damage_than_a_server_may_send` in
  `src/hvdisplay/tests.rs`, built like
  `the_stream_refuses_a_configure_wider_than_the_limit`. A raw server made
  with `vmlab_hv_display::{receive, send}` answers the Hello and sends a
  valid configure of 4 x 4, stride 16, one 64-byte memfd. Then it sends one
  `FrameReady` with 70000 rectangles, each `0,0 4x4`. Assert that
  `NativeStream::next_update` returns an error. Today it returns `Ok` with
  70000 `Rect` events. A second case asserts a limit on the sum: 4096 x 4096
  (64 MiB), and 16 full rectangles in one frame, must be an error. Today the
  call reads 1 GiB.
- Confidence: high.

### F9. The console TCP bridge opens the console to every local user (security)

- Where: `src/cli/console.rs:125-162` (`bridge`), `63-74` (the `--tcp`
  path), `85-89` and `98-119` (the detached helper), `src/viewer.rs:167-177`
  (which viewers get a bridge), `display/rfb.rs:87-91` (security type None).
- Why it is a fault: F7's fix made `vnc.sock` mode 0600 and made the accept
  loop refuse a peer of another user (`vmm/display.rs:742-778`), because a
  client of the RFB socket types keys and reads the screen without
  authenticating. `bridge` binds `127.0.0.1:5900` (or the next free port up
  to 5999, then any port) and copies every TCP connection to `vnc.sock`. It
  asks the TCP peer for nothing. The bridge process is the owner of the
  machine, so the peer check passes for every connection it carries. For as
  long as the bridge lives, every user of the host, and every process that
  can reach the host's loopback address, has the keyboard, the pointer and
  the screen of the guest. The bridge lives as long as the viewer window
  (the detached `__vncbridge` helper), or until Ctrl-C with `--tcp`. It is
  not a rare path: it is what `vmlab console` and `gui = true` use whenever
  the native viewer is not found and one of `remote-viewer`, `gvncviewer` or
  `vncviewer` is on `PATH`, and what `--tcp` always uses. The same holds for
  a QEMU machine, whose `vnc.sock` is protected by the 0700 runtime
  directory only. The manual describes the bridge
  (`docs/manual/pages/commands/console.wcl:67-80`) and does not say that it
  is open to other users.
- Reach: another local user, or a sandboxed process of the same user that
  may use loopback, on a host where a bridged viewer is open. The first
  review rated F7 as limited to a layout outside vmlab. This one is inside
  vmlab's own supported path.
- Proof: test `the_console_bridge_does_not_serve_an_unknown_tcp_peer` in
  `src/cli/console.rs`. A fake `vnc.sock` (a `UnixListener` in a temporary
  directory) counts its accepted connections and writes `RFB 003.008\n` to
  each. Call `bridge(&sock)`, connect a plain `TcpStream` to the returned
  port, send nothing that a vmlab-started viewer would be given (a token, a
  cookie), and read. Assert that the read ends without the RFB greeting and
  that the fake socket accepted no connection. Today the greeting arrives. A
  test cannot connect as another user, so it proves the missing check, not
  the cross-user access.
- Confidence: high.

### F10. Keys and buttons stay down after a display client disconnects (wrong result)

- Where: `display/core.rs:572-632` (`serve_native`, the end of a
  connection), `633-701` (`native_input`), `display/rfb.rs:111-120` and
  `122-190` (the RFB reader and its end), `vmm/display.rs:549-585`
  (`deliver`), `viewer/shell.rs:451-455` and `503-504` (the viewer's own
  release, on an orderly exit only).
- Why it is a fault: a client presses a key with `Key { down: true }` or an
  RFB `KeyEvent`, and a button with a `Buttons` mask or a `PointerEvent`.
  The server forwards each transition to the guest device and keeps no
  record of what a connection holds down. When the connection ends, by a
  close, an error, or the server's own revocation of F4's fix, nothing
  releases them. The guest keeps the key or the button down until some
  client presses and releases the same one. vmlab's own viewer sends the
  releases itself when it leaves in order (`App::ungrab` at the end of
  `run`) and after a reconnect (`pending_releases`). It cannot when it is
  killed, when it crashes (first-pass N3), or when its terminal closes: the
  viewer is started without a session of its own (N24). A third-party VNC
  viewer on `vnc.sock` is the common case: closing its window with a key
  chord sends the modifier down and then disconnects. After that the guest
  has Alt or Ctrl down. Scripted input shares the keyboard, so a later
  `type_text` of a script types Alt or Ctrl chords, and a held pointer
  button turns every scripted move into a drag.
- Reach: an ordinary user who closes or kills a viewer while a key or a
  button is down.
- Proof: test `a_disconnect_releases_what_the_client_held` in
  `hv/crates/vmlab-hv-display/tests/native.rs`, with an `InputSink` that
  sends each message into a channel. A client with `INPUT` sends
  `Key { qcode: "alt", down: true }` and `Buttons { mask: 1 }`, the test
  receives both from the channel, and the client closes. Assert that the
  channel then yields `Key { qcode: "alt", down: false }` and
  `Buttons { mask: 0 }`. Today the channel yields nothing more and the test
  does not end. The same test in `tests/rfb.rs`: an RFB client sends a
  `KeyEvent` down for keysym `0xffe9` and a `PointerEvent` with mask 1, and
  closes. A third case asserts that a key held by a second, still connected
  client is not released.
- Confidence: high.

### F11. hvdisplay: the handshake has no timeout and holds the input lock (wrong result)

- Where: `src/hvdisplay.rs:42-64` (`connect`), `157-169` (the capture sets
  its deadline after `connect` returns), `235-254` (`InputConn::send`),
  `274-279` (`NativeStream::connect`), `display/client.rs:18-27`
  (`Client::from_stream`: one `receive` with no timeout),
  `src/labd/hypervisor/vmlab_hv/display.rs:53-63` (the input mutex is held
  inside `spawn_blocking`).
- Why it is a fault: `connect` retries a socket that is not there for 3
  seconds. Once the socket takes the connection, `Client::from_stream` sends
  the Hello and waits for the reply with no read timeout. A unix socket
  takes a connection as soon as the listener exists, whether or not the
  process ever calls `accept`. So a vmlab-hv that is stopped (`SIGSTOP`, a
  debugger), that hangs, or whose connection worker waits in
  `self.publish()` before it replies (`display/core.rs:585-595`) holds the
  caller for ever. The first pass said of the capture "uses one deadline and
  is right" (N9). The deadline starts on line 159, after the handshake, so
  it does not cover this wait. Three callers are affected. A screenshot, an
  OCR or an image match never returns, and each holds one thread of the
  blocking pool. A recording never starts. Scripted input is the worst: the
  thread that waits holds `Mutex<InputConn>`, so every later key or pointer
  call of that machine waits for the mutex in a blocking-pool thread of its
  own, and a timeout that a caller puts around the call frees the caller and
  not the thread or the mutex. The viewer does this right: it sets a read
  and a write timeout before the handshake
  (`viewer/connection.rs:222-225`).
- Reach: a hypervisor process that does not answer. A hostile peer can do
  it on purpose. A hung or stopped vmlab-hv does it by accident.
- Proof: test `a_capture_gives_up_on_a_server_that_never_answers_the_hello`
  in `src/hvdisplay/tests.rs`. Bind a `UnixListener` in a temporary
  directory. A server thread accepts, reads the Hello with
  `vmlab_hv_display::receive`, sends nothing, and then reads again until the
  end of the stream. Call `capture_blocking(&path, Duration::from_millis(50))`
  and assert an error. Then join the server thread: its second read ends
  when the client closes. Today `capture_blocking` does not return. A second
  test for the lock: one `InputConn::send` against that server on a thread,
  and after the server has read its Hello, assert that the `Mutex<InputConn>`
  of an `HvDisplay` can be locked again once the first call gave up.
- Confidence: high for the missing timeout. Which callers above put a
  timeout around a display call was not checked.

### F12. The console TCP bridge does not pass a close (wrong result)

- Where: `src/cli/console.rs:150-157`, `display/core.rs:296-337` (the
  session list that both display sockets share), `display/rfb.rs:82`.
- Why it is a fault: each bridged connection runs two `tokio::io::copy`
  calls under `tokio::join!`. `copy` does not shut its writer down, and
  `join!` ends only when both ends are done. So a close on one side is not
  passed to the other. Two effects follow.
  - The VM restarts or stops: the unix side closes, and the copy to the TCP
    side ends. The TCP connection stays open, because the other copy still
    waits for bytes from the viewer. A viewer that has its one update
    request outstanding sends nothing by itself. It shows the last picture
    for ever and never learns that the server is gone, so it does not
    reconnect.
  - The viewer closes: the copy from TCP ends at the end of the stream, and
    the unix connection stays open until the server writes to it twice. The
    RFB server writes only to answer a request, and an incremental request
    is answered only when the screen, the cursor or the LEDs change. While
    the guest's screen does not change, the session stays in the display
    server. It counts as a CPU client (`needs_cpu()`, first-pass N5), and it
    takes one of the 64 places of `MAX_SESSIONS`. That list is shared with
    `display.sock`, so with `vmlab console --tcp`, which serves many viewer
    connections over its life, 64 such leftovers refuse every new display
    connection, vmlab's own screenshots and input included.
- Reach: an ordinary user of a bridged viewer.
- Proof: two tests in `src/cli/console.rs`, with a fake `vnc.sock` (a
  `UnixListener` in a temporary directory). `the_bridge_passes_a_server_close_to_the_viewer`:
  call `bridge`, connect a `TcpStream`, accept on the unix side and drop
  that stream. Assert that a read on the TCP stream returns `Ok(0)`. Today
  the read does not return. `the_bridge_passes_a_viewer_close_to_the_server`:
  connect, accept, drop the TCP stream, and assert that a read on the
  accepted unix stream returns `Ok(0)`. Today it does not return.
- Confidence: high for the first effect. Medium for the count of 64: it
  needs a guest whose screen stays the same between the viewer sessions.

### F13. The viewer replaces a non-text host clipboard at focus (wrong result)

- Where: `viewer/clipboard.rs:109-115` (`focus` forgets what it knew),
  `156-189` (`synchronize`), `170-174` (a host clipboard without text is
  passed over).
- Why it is a fault: when the window gets the focus, `last_host` and
  `last_poll` are cleared. `synchronize` then reads the host clipboard. If
  it holds an image, files, or nothing, `arboard` answers
  `ContentNotAvailable`, and the code goes on without touching `last_host`.
  `last_poll` is empty, so the guest's clipboard is read next. Its text
  differs from `last_host` (which is `None`), so `host.set(text)` runs. The
  host clipboard now holds the guest's text, whatever its age, and the image
  or the files that the user had copied are gone. An empty guest clipboard
  does the same with an empty string. The comment on line 162 says "Host
  changes win". Here the host changed and the guest did not, and the guest
  wins. First-pass N2 is the text case of the same function. This one needs
  no difference in the agent's answer: it happens on every focus.
- Reach: an ordinary user who copies an image or a file on the host and then
  clicks into the viewer window.
- Proof: test `focus_does_not_replace_a_host_clipboard_without_text` in the
  test module of `viewer/clipboard.rs`. The fake `Host` returns
  `arboard::Error::ContentNotAvailable` from `get` and records each `set`. A
  fake lab socket (a `UnixListener`, as the module's other tests use)
  answers `machine.clipboard_get` with `"old guest text"`. Call
  `bridge.focus(true)` and `bridge.tick(...)`. Assert that `set` was not
  called. Today it is called with the guest's text.
- Confidence: high.

### F14. A VGA mode of height 0 makes every publish fail (wrong result)

- Where: `hv/crates/vmlab-hv-devices/src/display/mod.rs:622-648` (`mode`:
  the 256-colour branch divides the height by the scan-line count),
  `282-287` (`scanout_active`), `display/core.rs:105-126` (`validate`
  refuses a height of 0), `384-391` (`capture`), `585-587` and
  `display/rfb.rs:94` (a new client captures before it gets an answer),
  `hv/crates/vmlab-hv-vmm/src/vm.rs:1311-1337` (`poll_display`).
- Why it is a fault: outside VBE the standard VGA computes its height as
  the vertical display end plus one, and in the 256-colour shift mode
  (`GC 5` bit 6) divides it by `(CRTC 9 & 31) + 1`. With a display end of 0
  and a scan-line count of 2 the height is `1 / 2 = 0`. The guest sets all
  of these registers with port writes, and a guest that writes the CRTC
  registers one at a time passes through such values during a mode set.
  `Source::frame` builds a frame of height 0, `validate` refuses it, and
  `capture` returns the error. `poll_display` runs every 16 ms and logs
  `display frame: invalid frame geometry, format, or damage` each time, with
  no limit on the rate: about 60 lines per second for as long as the guest
  stays in the mode. Clients that are connected keep the last picture and
  get no sign that it is stale. A new client with frames, and every RFB
  client, is closed in its handshake, because `serve_native` and `serve_rfb`
  call `publish()?` first. So a screenshot fails with a handshake error. The
  first pass said that the display server checks the frame geometry. It
  does, and the check has no path for a mode that is active and has no
  pixels.
- Reach: a guest with port access, on a machine with the standard VGA. A
  guest harms its own console and the machine's log only.
- Proof: two tests. `an_active_vga_mode_has_at_least_one_row` in
  `hv/crates/vmlab-hv-devices/src/display/tests.rs`: program a `StandardVga`
  through `io_write` so that `scanout_active()` is true, `GC 5` has bit 6,
  `CRTC 0x12` and the overflow bits are 0 and `CRTC 9` is 1. Assert
  `mode().height >= 1`. Today it is 0. `a_failing_source_does_not_refuse_a_new_client`
  in `hv/crates/vmlab-hv-display/tests/native.rs`: a `FrameSource` that
  gives one valid frame and then a frame of height 0. Start the server,
  connect a client with `FRAMES`, and assert that it gets its Hello reply
  and the last valid frame. Today the server closes it with `InvalidData`.
- Confidence: medium. The register values are from reading `mode()`. They
  were not written to a device. The Cirrus path
  (`display/cirrus/mod.rs:572-586`) has no division and was not checked
  further.

### F15. `gui = true` on a template build opens no viewer, and says the wrong thing (cosmetic)

- Where: `src/viewer.rs:298-317` (`open_when_ready`), `160-187`
  (`detect_vnc`: `remote-viewer` is `Transport::Tcp`), `9-14` (the module
  text), `src/template/build.rs:622-624` and `:1357`,
  `docs/vmlab-prd.md:937`.
- Why it is a fault: `open_when_ready` uses `detect()`, the VNC selection
  only, and starts a viewer only when its transport is `Unix`. `detect_vnc`
  gives `Unix` for `host.viewer` alone: all three known viewers, with
  `remote-viewer` first, are `Tcp`. So with no `host.viewer` the build
  prints "no unix-socket viewer for the build - install virt-viewer
  (remote-viewer) to watch builds". A user who installs it gets the same
  message again. The native viewer is never tried here, though every x86
  build VM runs on vmlab-hv and `open_for` would choose it
  (`src/viewer.rs:266-270`). The module text (lines 10-13) and the PRD
  (§11, line 937) still say that `remote-viewer` dials the unix socket
  directly, which the comment at `168-173` says it cannot.
- Reach: every template build with `gui = true` on a host without
  `host.viewer`.
- Proof: test `a_build_gui_finds_a_viewer_when_remote_viewer_is_installed`
  in the test module of `src/viewer.rs`. Give the selection that
  `open_when_ready` uses a `HostConfig::default()` and an `on_path` that
  knows `remote-viewer` only. Assert that it yields a viewer that
  `open_when_ready` starts. Today `detect_vnc` returns `Transport::Tcp` and
  the function prints the message. `open_when_ready` calls `detect()`
  directly, so the test needs the selection as a function of its inputs, as
  `select_for` is.
- Not checked: in which process the build runs. If it is the supervisor,
  the message goes to the daemon's output and the viewer would start with
  the daemon's environment.
- Confidence: high for the selection and the message.

## Notes

These have no test that I can name with confidence, were not confirmed, or
are choices and not faults.

- N16. First-pass N8, N9, N10 and N12 still hold as written. N9's sentence
  about `capture_blocking` is corrected by F11.
- N17. F5's fix left one path: a `CpuCopy` while the current frame is
  `Pixels::Leased` adds 1 to `state.seq` outside the capture lock
  (`display/core.rs:669-673`). `capture` reads `seq`, calls
  `source.frame()`, which consumes the source's damage, and `replace` then
  drops the frame without a word when `seq` has changed (`384-397`). On a
  machine where both a polled source and a leased GPU frame exist, a
  `CpuCopy` between the two steps loses that damage. This is the GPU path,
  which this card leaves out, so it was not followed further.
- N18. Each pointer move, click or drag of a script on a vmlab-hv machine
  takes a full screen capture first, only to learn the screen size
  (`src/labd/display/mod.rs:154-157`, `238`, `289`). The capture connects,
  makes the server allocate three buffers and copy the frame, converts it to
  RGB, writes a PNG and reads it back (`137-146`,
  `src/labd/hypervisor/vmlab_hv/display.rs:70-73`). While the guest's
  display is not active the capture waits 10 s and fails
  (`src/hvdisplay.rs:182-185`), and so does the click.
- N19. `screendump` encodes the PNG, and `grab_frames` decodes it, on the
  async runtime thread (`src/labd/hypervisor/vmlab_hv/display.rs:72`,
  `src/labd/display/mod.rs:145`). A script that polls OCR on a large screen
  keeps a runtime worker busy with each one. Not measured.
- N20. A capture of a vmlab-hv machine with a `gpu {}` goes over RFB
  (`src/labd/display/mod.rs:123-126`), not over the native protocol. That
  path returns the placeholder picture "Display output is not active." as a
  screenshot, where the native capture waits for a real frame
  (`src/hvdisplay.rs:182-185`). OCR and image matching then read the
  placeholder. It also has first-pass N4: a resize during the capture ends
  it with an error. GPU path, not followed further.
- N21. `HvDisplay::button` changes the held-button mask before it sends
  (`src/labd/hypervisor/vmlab_hv/display.rs:100-112`). A press that fails to
  send leaves its bit set, and a later press of another button sends both.
  `send_keys` sends the presses, waits 100 ms and sends the releases
  (`78-88`), and `mouse_click` and `mouse_drag` do the same with buttons. A
  caller that is dropped in that wait, or a release that fails, leaves the
  key or the button down in the guest. Whether a caller is ever dropped
  there was not checked.
- N22. The RFB reader sends a `Buttons` message after every `PointerAbs`,
  also when the mask did not change (`display/rfb.rs:158-170`). So two
  pending RFB moves never merge in the input queue
  (`display/input.rs:30-35`), and each move offers two reports to the
  tablet.
- N23. The viewer forwards a key only while the keyboard is grabbed
  (`viewer/input.rs:247-249`), and a grab on Wayland needs
  `zwp_keyboard_shortcuts_inhibit_manager_v1` (`viewer/grab.rs:110-112`).
  On a compositor without it the grab fails at every click, the error is
  printed, and no key ever reaches the guest. Which compositors lack the
  protocol was not checked.
- N24. The native viewer is started with the caller's terminal and session
  (`src/viewer.rs:54-60`), where the VNC bridge helper gets a session of its
  own and no terminal (`238-260`). Closing the terminal ends the viewer, its
  messages appear in that terminal, and a viewer that ends this way sends no
  key release (F10).
- N25. `spawn_child` splits the viewer command at white space after it put
  the target in (`src/viewer.rs:206-220`). A socket path with a space, or a
  `host.viewer` with a quoted argument, is cut into wrong arguments. The
  runtime directory rarely has a space.
- N26. When none of the ports 5900 to 5999 is free the bridge takes any
  port, and `display = port - 5900` wraps (`src/cli/console.rs:135-142`).
  `gvncviewer` and `vncviewer` are then given a display number that does not
  exist (`src/viewer.rs:192-198`).
- N27. The viewer clamps a pointer position in the letterbox border to the
  nearest edge of the guest screen (`viewer/input.rs:282-296`), and
  `pointer_inside` is about the window, not the picture. A click on the
  black border is a click on the edge pixel of the guest. Horizontal wheel
  steps are dropped (`viewer/shell.rs:296-299`, `vmm/display.rs:513`).
- N28. The viewer's reader thread needs a pidfd of the server process
  (`viewer/connection.rs:95-107`). Where the peer's pid is not visible
  (another pid namespace) `pidfd_open` fails, the error is not one that the
  start-up retries (`336-349`), and the viewer ends. Not confirmed.
- N29. When the import of a dma-buf frame fails, the viewer sends `CpuCopy`
  for that frame's surface (`viewer/shell.rs:173-178`). If the server has
  sent a newer configure by then, it closes the connection for a stale
  surface (`display/core.rs:661-663`). The viewer reconnects and asks for
  dma-buf again, because it does not remember the failure
  (`viewer/connection.rs:219-228`). On a host whose import always fails this
  can repeat. GPU path, not confirmed.
- N30. Over the VNC input transport, `type_text` sends a capital letter as
  Shift and the keysym of the small letter
  (`src/labd/display/keymap.rs:52-55`, `98-103`). QEMU's VNC server has a
  CapsLock correction that looks at the case of a letter keysym and the
  state of Shift. Whether it acts on this pair was not checked: it needs a
  run against QEMU. Since QEMU runs no x86 guest, only an aarch64 or riscv64
  guest whose profile selects the VNC transport takes this path.

## What I read, and what I did not

Read in full, without the test modules:

- `docs/reviews/display.md`
- `src/hvdisplay.rs`, `src/vnc.rs` (from line 60), `src/viewer.rs`,
  `src/cli/console.rs`, `src/labd/display/mod.rs`,
  `src/labd/display/keymap.rs`, `src/labd/hypervisor/vmlab_hv/display.rs`
- `display/core.rs`, `protocol.rs`, `transport.rs`, `client.rs`, `input.rs`,
  `lease.rs` (to line 125), `lib.rs`, and `rfb.rs` (to line 340: the keysym
  table was not read)
- `viewer/lib.rs`, `connection.rs` (to line 350), `shell.rs`,
  `clipboard.rs` (to line 265), `grab.rs`, `main.rs`, `input.rs` (the
  scancode table from line 61 to 149 was not read)
- `vmm/display.rs` (to line 803)

Read in part:

- `hv/crates/vmlab-hv-vmm/src/vm.rs`: `poll_display` and the two shutdown
  sites (1311-1337, 1683-1688, 1726-1728)
- `hv/crates/vmlab-hv-devices/src/display/mod.rs`: `enabled`,
  `scanout_active`, `dispi_write`, `mode` and the head of `frame`.
  `display/cirrus/mod.rs`: `mode`. The cursor checks of the two cursor
  sources, by their lines only.
- `hv/crates/vmlab-hv-display/PROTOCOL.md` (lines 255-310)
- `src/labd/vm.rs`: the `DisplayHost` implementation (2280-2307)
- `src/template/build.rs` and `src/cli/lab.rs`: the call sites of the viewer

Not read:

- `viewer/render.rs` (the Vulkan renderer), the shaders, `display/trace.rs`
  beyond its head
- every test module, `src/vnc/fake.rs`, `src/hvdisplay/fake.rs`, and the
  tests under `hv/crates/vmlab-hv-display/tests/`, apart from the names that
  the first review gives
- `src/record/` (the recorder that reads both streams). It has a review of
  its own, `docs/reviews/record-vision.md`.
- the GPU device and its scanout, by the terms of the card. N17, N20 and N29
  touch it and stop there.
- the TCG crates
- QEMU's VNC server and the `-vnc` argument (N30 depends on it)
- the lab daemon side of the clipboard verbs and the agents' clipboard code
- the USB tablet and keyboard devices and the PS/2 controller, apart from
  the list of key names that they know
- the Windows and Wayland viewer tests and the Python RFB client

## Other work seen

- `docs/manual/pages/commands/console.wcl` describes the TCP bridge and
  should say who can reach it, with F9.
