# Snapshots, screens, input and vision

## Snapshots

A snapshot captures a machine so it can be returned to later. vmlab takes them
online or offline, per machine or across a whole lab, and restores each to the
power state it was captured in. Verbs: see cli-machine.md.

### Online and offline

An **offline** snapshot, taken while the machine is powered off, captures disk
state only. An **online** snapshot, taken while it runs, captures disk, RAM and
device state, and pauses the guest only for as long as the save takes.

Every snapshot records the machine's power state at capture. Restoring an
online snapshot resumes the machine exactly where it was; restoring an offline
one leaves it powered off. A machine mid-transition, still starting or
stopping, is refused until it settles.

Both kinds are **qcow2-internal** snapshots: the on-disk footprint is the clone
file itself and the linked-clone backing chain is untouched. An online snapshot
is written through QMP's `snapshot-save` into the primary disk, covering every
disk the machine has; an offline one is written with `qemu-img` to each disk in
turn. Snapshots are named, listed and deleted per machine.

Host USB devices passed through with `vmlab usb` are not in a snapshot. An
online snapshot of a VM holding any is refused, naming them, until they are
detached (`vmlab usb detach <vm> --all`); an online restore detaches every
device the VM holds first, since the restored guest never saw them. Offline
snapshots and restores are unaffected.

When an online snapshot is restored, the daemon drops its agent connection
before resuming, because the rewound guest replays serial bytes the host
already consumed, and reconnects on next use.

A restored guest resumes with the clock it was captured with. After an online
restore the daemon waits up to 60 s for the agent and sets the guest clock to
the host's time (agent feature `set_time`). If it cannot (no agent answered, the
agent predates `set_time`, or the guest refused), the restore still succeeds and
`vmlab snapshot restore` prints a `WARNING:` line naming the machine and the
reason; the
`snapshot.restored` event's `clock` field says `set`, `kept`, `fixed` or
`not_set`. A machine's `clock` attribute opts out: `snapshot` keeps the
captured time, and an RFC 3339 date-time starts the machine at that instant
and is never set. An agent baked into a template before `set_time` existed is
replaced by `vmlab machine repair-agent` or by `up`'s automatic agent update.
The legacy agent sets the clock too; the TempleOS agent does not.

The RTC holds UTC. Windows guests on `vmlab-agent` read it as UTC because the
bootstrap installer sets `RealTimeIsUniversal = 1` and the agent re-ensures it
at every start; an older template gets it from `up`'s agent update or
`vmlab machine repair-agent`, after which the daemon sets the running clock
once (`clock = "host"`), the flag holding from the next boot. Profiles with
`rtc = "localtime"` (`windows-xp`, `windows-9x`) start the RTC at the host's
local time instead (`-rtc base=localtime`; a fixed instant converted to host
local time). Such a guest is right when its time zone matches the host's. vmlab
sets no guest time zone.

Containers snapshot with the same verbs and the same semantics. The
per-container scratch disk holds the snapshot, the image's read-only root is
outside it, and each snapshot records the image digest it was taken against
(the image pin is described in containers.md).

### Lab-wide snapshots

`vmlab snapshot create <name>` with no `--vm` captures every VM and container
in the lab under one name; `restore` with no `--vm` restores them all.
Consistency across machines is best-effort: each machine is captured in turn,
not at one coordinated instant, so two machines mid-conversation may land a few
moments apart. A test that depends on cross-machine consistency should quiesce
the guests first.

### What a snapshot does not hold

Warning — shares, volumes and workspaces are host state: a shared folder's
contents, a container volume's contents, and a dev machine's workspace all live
on the host. A restore never rolls any of them back. Only what is on the
machine's own disk, and in its RAM for an online snapshot, returns to the
captured state.

Shares stay snapshot-compatible on both transports: SMB carries no device
state, and a virtiofs share's daemon transfers its session state through the
snapshot's migration stream, so an online restore finds its open handles
intact. The files are whatever they are on the host now. The same holds for a
container's volumes.

### Snapshots and a dev workspace

A dev machine (dev-machines.md) holds a **workspace**: a guest-local working
copy of a host directory that is canonical, kept in step both ways by vmlab's
syncer. A restore rewinds that guest copy by hundreds of files at once, and a
naive bidirectional syncer cannot tell that apart from the developer having
edited hundreds of files; it would carry the old versions onto the canonical
copy silently. Because vmlab performs the restore, it **brackets** it. The
bracket has two asymmetric halves.

#### Capture flushes, and refuses with no escape

Before a capture, the syncer flushes the workspace so the snapshot is coherent
with the host tree. If the guest still holds work the canonical copy has never
seen, the capture **refuses**, naming the outstanding paths — the first twenty
and then how many more. Any of these refuses it:

- a standing conflict halt;
- a pass that could not finish because the guest stopped answering;
- a pending rescan or re-seed;
- paths the flush did not carry;
- a workspace that has never completed a pass at all.

A stopped machine is judged from its ledger, and refuses on the two states the
ledger still records: a halt, and an owed re-seed.

There is no flag to override this. A snapshot of a tree the host has never seen
restores somewhere meaningless, holding a version of the repository that exists
nowhere else. The fix is to let the flush finish or to resolve the halt. A
named skip, such as a socket in the tree or a root-owned build artefact, is
permanent and normal and does not refuse.

#### Restore takes the syncer off, rewinds, and re-seeds

A restore suspends the syncer before anything is rolled back, so no pass
already scanning can finish against a guest rewound underneath it. It then
restores the machine and puts the syncer back owing a **re-seed**: a host-only,
digest-based reconcile that carries the guest tree back to the canonical copy.

- **Host-only.** The re-seed emits no guest-to-host action at all, so
  rolled-back state cannot reach the canonical copy. The guest tree is walked
  only to decide what to overwrite and delete.
- **By digest.** A restored guest's clock runs behind the host, so a same-size
  in-place write compares identical on size and mtime. The re-seed compares
  content, never timestamps.
- **Before the watch reopens.** Otherwise the syncer's own writes would fill
  the fresh dirty set with thousands of self-inflicted paths.

The re-seed replaces the stat-walk the syncer would otherwise run on a watch
discontinuity, rather than following one: the walk asks what the guest did
while nobody was looking, and here vmlab already knows. That a re-seed is owed
rides the ledger in the lab's `.vmlab/`, because a restore does not need a
running machine; a stopped dev machine restored now re-seeds when it next
starts.

#### The one escape flag

A restore refuses while a conflict **halt** stands on the workspace, because a
restore discards the guest side of every halted path by design, and each of
those paths is a file you were going to be asked about. The refusal has one
escape: `--discard-guest-changes`, or `restore_discarding_workspace` from a
script. Passing it throws away the guest copy of every conflicting path and
lets the restore proceed. A restore that is refused leaves the machine exactly
as it was.

Warning — snapshots are not a workspace backup: a dev machine's source lives on
the host, which is what survives `destroy` and what a restore re-converges the
guest from. A snapshot holds the guest copy of the workspace as it was, and a
restore replaces it with the host's current tree. Every surface that takes or
restores a snapshot repeats this sentence.

### Snapshots under a freeze

Freezing the main lab for worktree labs (worktrees.md) changes what two kinds of
snapshot are. The snapshot frozen at becomes the **freeze point**: the state of the
frozen files every lab of the family stands on, which any of those labs restores
instantly by emptying its overlays. Every snapshot the main lab took before the
freeze is stored inside a frozen file, so it is **locked**: restoring, deleting or
taking it again would write a file other labs read, and each is refused until
`vmlab worktree thaw`. `vmlab snapshot list` marks both in its `FREEZE` column.
Snapshots taken after the freeze live in each lab's own overlays and behave as
above, except that an online one blocks a freeze-point restore and a thaw until it
is deleted. cli-machine.md (`vmlab snapshot`) gives the refusals.

### Headroom

Linked clones grow as guests write, and internal snapshots grow with them. The
supervisor watches free space on the filesystems holding `.vmlab/` and the
store and emits `host.disk_low` below the threshold set by `disk_low_percent`
in the host configuration (host-profiles.md), which an event handler can act on
(automation.md).

## Screens, input and vision

A VM always has a display, served over VNC on a unix socket, whether or not a
viewer is attached. That lets a wscript drive a guest before it has an agent:
type into an installer, wait for a dialog, click a button found by image. The
methods are all on the `Machine` handle; signatures are in
wscript-machine-api.md. On a machine with no display — a VM built with none, or
a container — every method here fails at call time with
"machine `name` has no display".

### How input reaches the guest

Scripted keyboard and mouse input goes through one of two transports, chosen by
the machine's profile (host-profiles.md). The default,
`input_transport = "qmp"`, sends QMP `send-key` events, which drive the PS/2
keyboard. The alternative, `"vnc"`, connects to the VM's own VNC socket and
sends keysyms and pointer events the way a real viewer does. The shipped
`windows-9x` profile uses VNC because real-mode DOS and Windows 9x setup
screens drop QMP key events between menu redraws; a USB-HID-only guest that
ignores the PS/2 keyboard also needs it. The script API is the same either way.
Input over the screen is how a DOS or 9x guest gets its legacy agent installed in
the first place; once it answers, `exec` is available on that guest too (see
host-profiles.md, `agent_transport`).

### Key chords

`m.send_keys("ctrl-alt-del")` presses a chord: the key names joined by `-`, all
pressed together for one event. Names are case-insensitive and follow QMP
`send-key` naming with common aliases:

| Name | Accepted spellings |
| --- | --- |
| modifiers | `ctrl` / `control`, `alt`, `shift`, `win` / `super` / `meta` |
| editing | `enter` / `return`, `tab`, `backspace`, `space`, `esc` / `escape`, `del` / `delete`, `insert` |
| navigation | `up`, `down`, `left`, `right`, `home`, `end`, `pgup` / `pageup`, `pgdn` / `pagedown` |
| function keys | `f1` to `f12` |
| single characters | any one letter or digit, e.g. `ctrl-c`, `alt-f4` |
| others | `menu`, `print`, `pause`, `caps_lock`, `num_lock`, `scroll_lock` |

An unknown name fails with "unknown key `x` in chord"; an empty chord is
refused. Because `-` is the separator, the minus key itself is typed with
`type_text("-")` rather than sent as a chord.

### Typing text

`m.type_text(text)` types literal text one character at a time, pausing 35 ms
between characters. `m.type_text_paced(text, delay_ms)` sets the pause, for
installers that drop keys when they arrive too fast. A newline in the text
presses Enter and a tab presses Tab. The keyboard map is US layout: letters,
digits and the shifted punctuation on a US keyboard all work, and any other
character fails with "cannot type character … (US layout only)". Uppercase
letters and symbols are sent as shift chords, so the guest must be on a US
layout too or they arrive wrong.

```ws
let k = vm.send_keys("ctrl-alt-del")
let t = vm.type_text("Password1!\n")
```

### The mouse

`m.mouse_move(x, y)` moves the pointer to absolute screen coordinates.
`m.mouse_click(button)` clicks `"left"`, `"right"` or `"middle"` at the
position the preceding move set; the handle remembers the last pointer
position, so a click never needs coordinates of its own.
`m.mouse_drag(x1, y1, x2, y2)` presses the left button at the first point,
moves in a few steps, and releases at the second. All three go through
whichever input transport the profile chose.

### Screenshots

`m.screenshot(path)` writes the current framebuffer as a PNG and returns the
path it wrote. With an empty string it chooses a name of the form
`<machine>-<timestamp>.png` under `screenshots/` in the lab's own `.vmlab/`
directory; with a relative path it writes beside the running script. QEMU's own
screendump is a PPM, and the image loader sniffs formats by content rather than
extension, so a screenshot, a PNG crop and a BMP are all accepted anywhere a
reference image is expected.

The framebuffer comes from QEMU's `screendump`, except on a VM whose
`gpu.mode` is `virgl` or `vulkan`. There the guest's screen is a GL texture on
the host, which `screendump` cannot read (`no surface`), so every capture —
`screenshot`, `ocr`, `find_image`, the waits, and the `vmlab vm` screen verbs —
is one full RFB update read over the VM's own VNC socket instead. A VM whose
`screendump` reports `no surface` for any other reason is retried over VNC
once; if that fails too, the error names both.

### Template matching

Template matching finds a small reference image inside the screen. The
reference is a crop taken once, by hand, of the thing to wait for: an "Install
now" button, a login prompt, a completed progress bar. By convention reference
images live in a directory beside the script, and a relative path resolves
against the script's directory.

- `m.wait_for_image(ref, timeout_secs)` grabs the screen once a second until
  the reference appears, and returns a `Match` or an error saying how long it
  waited and on which machine.
- `m.wait_for_any([refs], timeout_secs)` does the same for several references
  and returns the first found.
- `m.find_image(ref)` is a single grab with no wait and returns
  `Option[Match]`.
- `m.wait_for_image_opts(ref, timeout_secs, threshold, region)` exposes the two
  options the others fix:

  - **threshold** is the minimum similarity score, default 0.9. Scores are
    zero-mean normalised cross-correlation values where 1.0 is a perfect match,
    compared against the threshold with no rescaling. Matching is done in
    grayscale.
  - **region** is `[x, y, w, h]` restricting the search to part of the screen,
    or an empty list for the whole screen. A list of any other length is an
    error. Coordinates in the returned `Match` are always absolute screen
    coordinates, even when a region was given.

A `Match` carries the top-left `x`, `y`, the template's `w`, `h`, the `score`,
and the centre `cx`, `cy`, so a found image anchors a click:

```ws
match vm.wait_for_image("images/install-now.png", 300) {
    Ok(m) => {
        let mv = vm.mouse_move(m.cx, m.cy)
        let cl = vm.mouse_click("left")
    }
    Err(e) => lab.log(e),
}
```

The search is two-stage so it stays fast on an HD screen: a coarse pass on a 4x
downscaled copy keeps the eight best candidates, and each is refined at full
resolution within a few pixels. A reference with no variance at all, a solid
colour, has no defined correlation and falls back to a mean absolute difference
score against the same threshold. Crop references with some texture in them,
and crop tightly: a reference larger than the screen or the region can never
match.

Matching is not scale-invariant. Take reference crops from a screenshot of the
same VM at the same resolution the script will run against, or the score never
reaches the threshold.

### OCR

`m.ocr()` returns the text on screen and `m.ocr_region([x, y, w, h])` the text
in one region, both as tesseract's output verbatim, trailing whitespace
included. vmlab drives the `tesseract` binary as a subprocess with page
segmentation mode 6, which assumes one uniform block of text and suits console
and dialog screenshots. If the binary is not on `PATH` the call fails with a
message telling you to install tesseract-ocr. A region whose origin lies
outside the image is an error; a region that runs past the edge is clipped.

`m.wait_for_text(pattern, timeout_secs)` grabs the screen once a second, runs
OCR over the whole of it and returns as soon as the regex matches. The returned
`Match` carries the matched text in `text` and zero for the position fields,
because OCR does not report where on screen it read a line. OCR is fuzzy:
different tesseract versions read edge glyphs differently, so match a
distinctive word rather than a whole sentence, and prefer image matching where a
stable reference can be cropped.

### Choosing between them

Once a guest has the agent, `exec` and `wait_ready` are faster and more
reliable than anything on the screen, and a VM without an agent never reports
ready, so a script targeting one relies on screen and time waits alone. Use the
screen for the part of a template build before the agent is installed and for
the rare interactive step no command line reaches. Image matching is exact and
fast but tied to one look; OCR survives theme and resolution changes but is
slower and approximate. Both are available in every GPU mode: with a GL
display the capture is read over VNC, which sees the host-rendered framebuffer.

## Recording

A console recording is a video of one VM's display, MP4 (H.264, `yuv420p`) or
WebM (VP9) by the output path's extension, written by the lab daemon from the
VM's VNC socket while it runs. Started by `vmlab record start`, `--record
<vm>=<path>` on `script`/`up`/`vm start`/`vm restart`, or `Machine.record` in a
script; the flags, defaults, refusals and lifecycle are in cli-machine.md
(`vmlab record`), the method in wscript-machine-api.md. What the video shows:

- **Frames.** Constant rate (default 30 fps, 1 to 120). Every source frame is
  scaled with aspect kept and letterboxed into one output size (default
  1920x1080; both dimensions even, 16 to 7680x4320), so a 640x480 boot screen
  and a later 1280x1024 desktop land in one frame size. A still screen repeats
  its last picture. No audio, no captions.
- **The pointer rule.** QEMU never sends pointer position over VNC, so the
  pointer is drawn at the last position vmlab itself sent (`mouse_move`,
  `mouse_click`, `mouse_drag` from a script or `vmlab vm mouse-move`/`click`/
  `drag`); a person's input through a `vmlab console` viewer is not tracked.
  It is drawn only when QEMU sends a cursor shape (virtio-vga/virtio-gpu, qxl,
  vmware); on std VGA, bochs and cirrus the guest paints its own pointer into
  the framebuffer, so nothing is drawn twice. A virtio-gpu guest hiding its
  cursor (full-screen video) still shows the last shape. Across a reboot or a
  resolution change QEMU re-announces the last shape with its hotspot zeroed;
  the recorder treats that as stale and hides the pointer until the guest
  defines a shape with its own hotspot (a shape whose hotspot is (0,0) stays
  hidden until a different one arrives), so no old pointer is drawn over
  firmware, kernel or login screens. Cursor pixels are blended as the
  premultiplied alpha the guest uploads.
- **Ripple.** An amber ripple at every click vmlab sends, and both ends of a
  drag, growing and fading over 0.5 s, drawn whether or not a cursor shape is
  known, beneath the pointer.
- **Idle compression** (`--compress-idle[=SECS]`, `compress_idle` in `opts`).
  An interval is idle when its framebuffer changes cover at most 64x64 px (a
  blinking caret or a seconds clock does not count; QEMU reports dirt in
  16-pixel cells) and vmlab sent no input. An idle run longer than the
  threshold (default 2 s) becomes 0.5 s of video. A click on a static screen
  is never cut. Off unless asked for.
- **Timeline** (`--timeline`). `<output>.events.json` beside the video:
  `{video, fps, size, duration, events: [...], cuts: [...]}`. Each event has
  `at` (video seconds, after cuts), `wall` (seconds since start) and `kind`:
  `keys{chord}`, `text{text}`, `move{x,y}`, `click{button,x?,y?}` (position
  only when the click moved first), `drag{x1,y1,x2,y2}`. Each cut is
  `{wall_start, wall_end, kept}`. Without idle compression `cuts` is empty and
  `at` equals `wall` on every entry. Only input vmlab sent appears.
- **Lifecycle.** Owned by the lab daemon: survives the CLI exiting, guest
  reboots, `vm restart` and snapshot restore (the last frame is held across
  the gap and the recorder reconnects to the new QEMU); finalised to a
  playable file on `record stop`, `Recording.stop`, `--max-duration`, `vm
  stop`, `down`, `destroy`, `vm destroy`, lab daemon exit, and the
  `host.disk_low` watchdog. One per machine (`conflict`). Events
  `record.started` and `record.stopped` (automation.md).
- **Limits.** No GIF (convert the MP4 with ffmpeg afterwards); no audio;
  containers have no display (`unsupported`); a human's pointer through a VNC
  viewer is not tracked; template build VMs cannot be recorded.
