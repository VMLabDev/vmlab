# wscript API: Machine

`Machine` is the handle a `Lab` gives out for a VM or a container (see
wscript-lab-api.md). There is one handle for both kinds. Every method exists on
every machine; what a particular machine cannot do is reported when the method
is called, and the error names the missing capability rather than the machine's
kind. A container with no display fails `screenshot` with
"machine `web` has no display", never with "no such method".

Method groups:

- Lifecycle and state: `name`, `kind`, `start`, `stop`, `stop_force`,
  `restart`, `poweroff`, `state`, `is_ready`, `wait_ready`, `is_healthy`,
  `agent_answering`, `wait_shutdown`, `ip`, `ip_nic`.
- Snapshots: `snapshot`, `restore`, `restore_discarding_workspace`,
  `snapshots`, `delete_snapshot`.
- Input: `send_keys`, `type_text`, `type_text_paced`, `mouse_move`,
  `mouse_click`, `mouse_drag`.
- Screen: `screenshot`, `wait_for_image`, `wait_for_image_opts`,
  `wait_for_any`, `find_image`, `ocr`, `ocr_region`, `wait_for_text`.
- Guest agent: `exec`, `exec_timeout`, `copy_to`, `copy_from`, `logs`,
  `terminal`, `stats`.
- Identity: `logins`, `as_login`, `as_account`.

Two conventions apply throughout. Relative host paths, for reference images,
screenshots and file transfers, resolve against the directory the running
script lives in, so a provision can ship its reference crops beside itself. And
every method that can fail returns a wscript `Result` with a string error; the
descriptions below say what the error carries. The signatures use the parameter
names the host registers; the generated interface file spells them `a0`, `a1`
and so on.

> Note — readiness inside a first-boot script: on the handle `lab.this_vm()`
> returns inside a template's first-boot provision, `is_ready` and `wait_ready`
> mean "the agent answers right now", because full readiness is deferred until
> that script returns. On every other handle they mean full readiness.

## Machine.name

The machine's name, as the lab file declares it.

```wscript
fn name(self) -> string
```

It never fails.

```wscript
fn main(lab: Lab) {
    for m in lab.machines() { lab.log(m.name()) }
}
```

## Machine.kind

Which kind of machine this is.

```wscript
fn kind(self) -> string
```

Returns `vm` or `container`. It never fails. Branch on a capability error
rather than on the kind where you can; see containers.md.

```wscript
fn main(lab: Lab) {
    let Ok(m) = lab.machine("web") else { return }
    lab.log(m.name() + " is a " + m.kind())
}
```

## Machine.start

Start the machine if it is stopped.

```wscript
fn start(self) -> Result[unit, string]
```

A machine that is not stopped returns `Ok` without doing anything. A pending
template or image download is completed first. The call returns once the
emulator is running; it does not wait for readiness, so follow it with
`Machine.wait_ready`. The error carries the reason the download or the boot
failed.

```wscript
fn main(lab: Lab) {
    let Ok(m) = lab.machine("api") else { return }
    let s = m.start()
    let r = m.wait_ready(300)
}
```

## Machine.stop

Stop the machine gracefully.

```wscript
fn stop(self) -> Result[unit, string]
```

Runs the stop ladder: for a VM an agent-requested shutdown, then an ACPI
power-down, then a hard kill, each rung with its own timeout. For a container
the ladder is an in-guest stop signal with a grace period, then an agent
shutdown, then a hard kill. A machine that is already stopped returns `Ok`. The
error carries what the ladder reported.

```wscript
fn main(lab: Lab) {
    let Ok(m) = lab.vm("dc01") else { return }
    match m.stop() {
        Ok(_) => lab.log("stopped"),
        Err(e) => lab.log("stop failed: " + e),
    }
}
```

## Machine.stop_force

Kill the machine immediately.

```wscript
fn stop_force(self) -> Result[unit, string]
```

Skips the ladder and sends the emulator a kill. Writes the guest had not
flushed are lost. Use `Machine.poweroff` for a guest that has no ACPI and needs
its disk sealed cleanly.

```wscript
fn main(lab: Lab) {
    let Ok(m) = lab.container("web") else { return }
    let r = m.stop_force()
}
```

## Machine.restart

Stop the machine gracefully, wait for it to settle, and start it again.

```wscript
fn restart(self) -> Result[unit, string]
```

Runs `Machine.stop`, waits up to sixty seconds for the power state to reach
stopped, then runs `Machine.start`. It fails with "<name> did not stop for
restart" when the machine does not settle in time, and otherwise with the stop
or start error. This restarts the emulator; a reboot the guest performs itself
is watched with `Machine.agent_answering` instead.

```wscript
fn main(lab: Lab) {
    let Ok(m) = lab.vm("member") else { return }
    let r = m.restart()
    let ready = m.wait_ready(600)
}
```

## Machine.poweroff

Exit the emulator cleanly, flushing disk caches first.

```wscript
fn poweroff(self) -> Result[unit, string]
```

Sends the emulator a clean quit and waits for the power state to settle. Unlike
`Machine.stop_force`, block-device caches are flushed before the process exits,
so this is the only safe way to seal a consistent disk for a guest with no
ACPI, such as DOS or Windows 3.x, where the ladder's last rung would otherwise
drop unflushed writes. The error carries the reason the machine did not settle.

```wscript
fn main(lab: Lab) {
    let Ok(m) = lab.vm("dos") else { return }
    let r = m.poweroff()
}
```

## Machine.state

The machine's power state.

```wscript
fn state(self) -> string
```

Returns `stopped`, `starting`, `running` or `stopping`. It never fails. Running
says nothing about readiness; see `Machine.is_ready`.

```wscript
fn main(lab: Lab) {
    let Ok(m) = lab.machine("api") else { return }
    if m.state() == "stopped" { let s = m.start() }
}
```

## Machine.is_ready

Whether the machine is fully usable.

```wscript
fn is_ready(self) -> bool
```

Ready means the agent has come up and any first-boot work has completed. The
flag is sticky: it stays set while the emulator runs, even through a reboot the
guest performs itself. Use `Machine.agent_answering` for a live signal. Inside
the machine's own first-boot provision this returns the live agent probe
instead. It never fails.

```wscript
fn main(lab: Lab) {
    let Ok(m) = lab.machine("api") else { return }
    if !m.is_ready() { let r = m.wait_ready(300) }
}
```

## Machine.wait_ready

Block until the machine is ready or the timeout passes.

```wscript
fn wait_ready(self, timeout_secs: int) -> Result[unit, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `timeout_secs` | int | How long to wait, in seconds. A negative value is treated as zero. |

Polls `Machine.is_ready` with the same first-boot gating. It fails with
"<name>: not ready after <timeout>" when the deadline passes, and with "<name>
stopped while waiting for ready" when the machine stops in the meantime.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else { return }
    match dc.wait_ready(600) {
        Ok(_) => lab.log("dc01 ready"),
        Err(e) => lab.log("not ready: " + e),
    }
}
```

## Machine.is_healthy

The latest verdict of the machine's healthcheck.

```wscript
fn is_healthy(self) -> bool
```

A machine that declares no healthcheck counts as healthy once it is ready, so a
script can gate on this for every machine. `vmlab machine capabilities` says
whether the machine has a healthcheck at all. It never fails.

```wscript
fn main(lab: Lab) {
    let Ok(web) = lab.container("web") else { return }
    if !web.is_healthy() { lab.log("web is unhealthy") }
}
```

## Machine.agent_answering

Whether the guest agent answers a ping right now.

```wscript
fn agent_answering(self) -> bool
```

The live probe, on every handle. It goes false while the guest is down or
mid-reboot even though `Machine.is_ready` stays true, which is what a build
provision needs to watch a reboot it asked the guest to perform. It never
fails.

```wscript
fn main(lab: Lab) {
    let Ok(m) = lab.vm("box") else { return }
    let r = m.exec("shutdown", ["/r", "/t", "0"])
    while m.agent_answering() { vmlab::sleep_ms(1000) }
    while !m.agent_answering() { vmlab::sleep_ms(2000) }
}
```

## Machine.wait_shutdown

Block until the machine's power state is stopped.

```wscript
fn wait_shutdown(self, timeout_secs: int) -> Result[unit, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `timeout_secs` | int | How long to wait, in seconds. |

Waits for the exit monitor to settle the state at stopped, however the machine
got there: a `Machine.stop`, a guest-initiated shutdown, or a crash. The error
says the state was not reached in time.

```wscript
fn main(lab: Lab) {
    let Ok(m) = lab.vm("box") else { return }
    let r = m.exec("shutdown", ["/s", "/t", "0"])
    let down = m.wait_shutdown(120)
}
```

## Machine.ip

The machine's first IPv4 address.

```wscript
fn ip(self) -> Result[string, string]
```

Asks the agent for its interfaces, matches them to the lab file's NIC order by
MAC address, and returns the first NIC that has an IPv4 address. It fails when
the agent is not reachable and with "<name>: no IPv4 address reported by agent"
when no NIC has one yet.

```wscript
fn main(lab: Lab) {
    let Ok(m) = lab.machine("api") else { return }
    match m.ip() {
        Ok(ip) => lab.log("api at " + ip),
        Err(e) => lab.log(e),
    }
}
```

## Machine.ip_nic

The IPv4 address of one NIC, by index.

```wscript
fn ip_nic(self, nic: int) -> Result[string, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `nic` | int | The NIC's position among the machine's `nic {}` blocks, starting at 0. A negative value is treated as 0. |

The same lookup as `Machine.ip`, restricted to one NIC. It fails with the same
messages, and the "no IPv4 address" error also covers an index beyond the last
NIC.

```wscript
fn main(lab: Lab) {
    let Ok(m) = lab.vm("router") else { return }
    let inside = m.ip_nic(1)
}
```

## Machine.snapshot

Take a snapshot, online if the machine is running and offline if not.

```wscript
fn snapshot(self, name: string) -> Result[unit, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `name` | string | The snapshot's name. |

A running machine gets an online snapshot that captures memory and resumes
exactly where it was on restore; a stopped one gets an offline disk snapshot. A
container's record also pins the image digest the capture is valid against. The
record is written to the lab's state file and a `snapshot.created` event is
emitted.

On a dev machine the workspace syncer flushes first, so the snapshot is
coherent with the canonical tree (see dev-machines.md). If the guest holds work
the host has never seen, the call refuses, and there is no flag to override it.
The error carries that refusal, the pin failure, or the emulator's own error.
Snapshots are not a workspace backup; see snapshots-vision.md.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else { return }
    match dc.snapshot("post-promote") {
        Ok(_) => lab.log("snapshot taken"),
        Err(e) => lab.log("snapshot refused: " + e),
    }
}
```

## Machine.restore

Restore a snapshot by name.

```wscript
fn restore(self, name: string) -> Result[unit, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `name` | string | The snapshot to restore. |

An online snapshot resumes running where it was; an offline one leaves the
machine off. A container's pinned image digest is checked first, and a mismatch
fails with a message naming both digests and the two ways out: destroy the
machine or restore the original pin. It fails with "<name> has no snapshot
<snap>" for an unknown name.

On a dev machine the syncer is taken off the workspace before the rewind and
put back owing a re-seed, which carries the canonical host copy back into the
guest and lets nothing flow the other way. A workspace that is halted on a
conflict refuses to restore, because the rewind would destroy the guest copy of
every conflicting path; that refusal is the one
`Machine.restore_discarding_workspace` overrides.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else { return }
    let r = dc.restore("post-promote")
    let ready = dc.wait_ready(300)
}
```

## Machine.restore_discarding_workspace

Restore a snapshot on a dev machine whose workspace is halted, discarding the
guest copy of every conflicting path.

```wscript
fn restore_discarding_workspace(self, name: string) -> Result[unit, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `name` | string | The snapshot to restore. |

The same restore as `Machine.restore` with the explicit discard flag set, the
counterpart of `vmlab snapshot restore --discard-guest-changes`. It is a
separate verb rather than a boolean argument so that the destruction is asked
for by name at the call site. On a machine without a workspace the flag means
nothing and the call behaves as `Machine.restore`. The errors are those of
`Machine.restore` minus the halt refusal.

> Warning — this destroys guest work: guest-side changes at every halted path
> are lost and are not recoverable from vmlab. Resolve the halt with `vmlab dev
> sync resolve` first if any of them matter.

```wscript
fn main(lab: Lab) {
    let Ok(dev) = lab.vm("dev") else { return }
    let r = dev.restore_discarding_workspace("clean")
}
```

## Machine.snapshots

The names of the machine's snapshots.

```wscript
fn snapshots(self) -> Result[List[string], string]
```

Returns the names recorded in the lab's state file. The full records, with when
each was taken and whether it is online, are what `vmlab snapshot list` prints.
It fails when the machine does not exist.

```wscript
fn main(lab: Lab) {
    let Ok(m) = lab.vm("dc01") else { return }
    match m.snapshots() {
        Ok(names) => { for n in names { lab.log(n) } }
        Err(e) => lab.log(e),
    }
}
```

## Machine.delete_snapshot

Delete a snapshot by name.

```wscript
fn delete_snapshot(self, name: string) -> Result[unit, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `name` | string | The snapshot to delete. |

Removes the snapshot from the disk and its record from the state file. No event
is emitted. The error carries the emulator's or the image tool's reason.

```wscript
fn main(lab: Lab) {
    let Ok(m) = lab.vm("dc01") else { return }
    let r = m.delete_snapshot("scratch")
}
```

## Machine.send_keys

Send one key chord to the machine's display.

```wscript
fn send_keys(self, chord: string) -> Result[unit, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `chord` | string | Key names joined with `-`, pressed together, for example `ctrl-alt-del` or `f2`. |

Key names are the emulator's own key codes with common aliases: `ctrl`, `alt`,
`shift`, `win`, `enter`, `esc`, `tab`, `space`, `backspace`, `del`, `up`,
`down`, `left`, `right`, `home`, `end`, `pageup`, `pagedown`, `insert`, `f1` to
`f12`, and any single letter or digit. Names are case-insensitive. It fails
with "machine <name> has no display" on a machine without one, with "unknown
key <name> in chord" for a name it does not know, and with "empty key chord"
for an empty string.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else { return }
    let k = dc.send_keys("ctrl-alt-del")
}
```

## Machine.type_text

Type literal text into the machine's display, one character at a time.

```wscript
fn type_text(self, text: string) -> Result[unit, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `text` | string | The characters to type. A newline is typed as Enter. |

Uses a US keyboard layout and pauses 35 milliseconds between characters.
Characters the layout cannot produce fail with a message naming the character.
It fails on a machine without a display.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else { return }
    let t = dc.type_text("Password1!\n")
}
```

## Machine.type_text_paced

Type literal text with a chosen pause between characters.

```wscript
fn type_text_paced(self, text: string, delay_ms: int) -> Result[unit, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `text` | string | The characters to type. |
| `delay_ms` | int | The pause between characters, in milliseconds. A negative value is treated as 0. |

`Machine.type_text` with the pacing exposed, for a guest that drops keystrokes
at the default pace. The errors are the same.

```wscript
fn main(lab: Lab) {
    let Ok(old) = lab.vm("win98") else { return }
    let t = old.type_text_paced("setup.exe\n", 120)
}
```

## Machine.mouse_move

Move the pointer to an absolute position on the display.

```wscript
fn mouse_move(self, x: int, y: int) -> Result[unit, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `x` | int | Horizontal position in framebuffer pixels, from the left. |
| `y` | int | Vertical position in framebuffer pixels, from the top. |

The position is remembered on the handle, and `Machine.mouse_click` reuses it.
The handle returned by `Machine.as_login` shares the same pointer, since it is
the same machine. It fails on a machine without a display.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else { return }
    let Ok(m) = dc.wait_for_image("images/ok-button.png", 60) else { return }
    let mv = dc.mouse_move(m.cx, m.cy)
    let cl = dc.mouse_click("left")
}
```

## Machine.mouse_click

Click a mouse button at the position the last move set.

```wscript
fn mouse_click(self, button: string) -> Result[unit, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `button` | string | `left`, `middle` or `right`. |

Presses and releases the button with a short pause between, at the position the
preceding `Machine.mouse_move` or `Machine.mouse_drag` left the pointer; with
no move yet the position is (0, 0). It fails on a machine without a display and
on an unknown button name.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else { return }
    let mv = dc.mouse_move(640, 400)
    let cl = dc.mouse_click("right")
}
```

## Machine.mouse_drag

Press the left button at one point, drag to another, and release.

```wscript
fn mouse_drag(self, x1: int, y1: int, x2: int, y2: int) -> Result[unit, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `x1` | int | Where to press, horizontal. |
| `y1` | int | Where to press, vertical. |
| `x2` | int | Where to release, horizontal. |
| `y2` | int | Where to release, vertical. |

The pointer moves in a few steps between the two points so the guest sees a
drag rather than a jump, and is left at the release point. It fails on a
machine without a display.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else { return }
    let d = dc.mouse_drag(100, 100, 400, 300)
}
```

## Machine.screenshot

Save a PNG of the machine's display and return where it was written.

```wscript
fn screenshot(self, path: string) -> Result[string, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `path` | string | The host file to write. Relative paths resolve against the script's directory. Empty picks a timestamped name under the lab's `.vmlab/screenshots/`. |

Returns the path written. Missing parent directories are created. With an empty
`path` the file is named `<machine>-<UTC timestamp>.png`. It fails on a machine
without a display and with the write error otherwise.

```wscript
fn handle(event: Event, lab: Lab) {
    if event.name != "vm.crashed" { return }
    let Ok(m) = lab.machine(event.vm) else { return }
    match m.screenshot("") {
        Ok(path) => lab.log("saved " + path),
        Err(e) => lab.log(e),
    }
}
```

## Machine.wait_for_image

Wait until a reference image appears on the display.

```wscript
fn wait_for_image(self, image: string, timeout_secs: int) -> Result[Match, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `image` | string | A PNG to look for. Relative paths resolve against the script's directory; by convention they live in `images/`. |
| `timeout_secs` | int | How long to keep looking, in seconds. |

Grabs the screen once a second and runs normalised template matching over the
whole of it with a similarity threshold of 0.9. Returns the first `Match`,
whose `cx` and `cy` anchor a click. It fails with "timed out after <n>s waiting
for [<image>] on <name>" when the deadline passes, with "reference image
<path>: <reason>" when the file cannot be loaded, and on a machine without a
display.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else { return }
    match dc.wait_for_image("images/login.png", 120) {
        Ok(m) => { let mv = dc.mouse_move(m.cx, m.cy) },
        Err(e) => lab.log(e),
    }
}
```

## Machine.wait_for_image_opts

`Machine.wait_for_image` with the threshold and search region exposed.

```wscript
fn wait_for_image_opts(self, image: string, timeout_secs: int, threshold: float, region: List[int]) -> Result[Match, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `image` | string | The reference PNG. |
| `timeout_secs` | int | How long to keep looking, in seconds. |
| `threshold` | float | The minimum similarity score, 0 to 1. The default elsewhere is 0.9. |
| `region` | List[int] | `[x, y, w, h]` to search only part of the screen, or `[]` for the whole screen. |

Behaves as `Machine.wait_for_image` otherwise. It also fails with "region needs
[x, y, w, h], got <n> elements" when `region` has any other length. Restricting
the region makes a match faster and keeps a small crop from matching in the
wrong place.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else { return }
    let m = dc.wait_for_image_opts("images/tray-icon.png", 60, 0.8, [1700, 1000, 220, 80])
}
```

## Machine.wait_for_any

Wait until any one of several reference images appears.

```wscript
fn wait_for_any(self, images: List[string], timeout_secs: int) -> Result[Match, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `images` | List[string] | The reference PNGs, tried in order on every grab. |
| `timeout_secs` | int | How long to keep looking, in seconds. |

On each grab the images are tried in list order and the first match wins, at
the default threshold over the whole screen. The returned `Match` does not say
which image matched; compare its position if that matters. The errors are those
of `Machine.wait_for_image`, with the whole list named in the timeout message.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else { return }
    let m = dc.wait_for_any(["images/desktop.png", "images/oobe.png"], 300)
}
```

## Machine.find_image

Look for a reference image once, without waiting.

```wscript
fn find_image(self, image: string) -> Result[Option[Match], string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `image` | string | The reference PNG. |

One grab, one match at the default threshold over the whole screen. Returns
`None` when nothing scores high enough; that is not an error. It fails when the
reference cannot be loaded and on a machine without a display.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else { return }
    match dc.find_image("images/update-prompt.png") {
        Ok(Some(m)) => { let mv = dc.mouse_move(m.cx, m.cy) let cl = dc.mouse_click("left") }
        Ok(None) => lab.log("no prompt"),
        Err(e) => lab.log(e),
    }
}
```

## Machine.ocr

Read the text on the whole display.

```wscript
fn ocr(self) -> Result[string, string]
```

Grabs the screen and runs it through the OCR engine. The result is the
recognised text in reading order, with the engine's line breaks. It fails on a
machine without a display and with the engine's error otherwise. See
snapshots-vision.md for what OCR does and does not read well.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else { return }
    match dc.ocr() {
        Ok(text) => lab.log(text),
        Err(e) => lab.log(e),
    }
}
```

## Machine.ocr_region

Read the text in one region of the display.

```wscript
fn ocr_region(self, region: List[int]) -> Result[string, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `region` | List[int] | `[x, y, w, h]` in framebuffer pixels, or `[]` for the whole screen. |

`Machine.ocr` restricted to a rectangle. It also fails with "region needs [x,
y, w, h], got <n> elements" for any other list length.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else { return }
    let title = dc.ocr_region([0, 0, 1024, 40])
}
```

## Machine.wait_for_text

Wait until OCR of the display matches a regular expression.

```wscript
fn wait_for_text(self, pattern: string, timeout_secs: int) -> Result[Match, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `pattern` | string | A regular expression, matched anywhere in the recognised text. |
| `timeout_secs` | int | How long to keep looking, in seconds. |

Grabs and OCRs the whole screen once a second. On a match it returns a `Match`
whose `text` field is the matched text; the position fields are 0 and the score
is 1, because OCR does not report where the words were. It fails with "bad
pattern: <reason>" for an invalid expression, with "timed out after <n>s
waiting for /<pattern>/ on <name>" at the deadline, and on a machine without a
display.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else { return }
    match dc.wait_for_text("Press Ctrl.Alt.Del", 300) {
        Ok(m) => lab.log("saw: " + m.text),
        Err(e) => lab.log(e),
    }
}
```

## Machine.exec

Run a command in the guest through the agent and collect its output.

```wscript
fn exec(self, cmd: string, args: List[string]) -> Result[ExecResult, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `cmd` | string | The program to run, as the guest resolves it. |
| `args` | List[string] | Its arguments, one per element, passed without shell parsing. |

Runs under the identity the handle carries: the agent identity on the handle a
lab gives out, or the login `Machine.as_login` or `Machine.as_account`
resolved. Output is streamed back and captured into an `ExecResult` with the
exit code, stdout and stderr. The command is given 120 seconds; use
`Machine.exec_timeout` for longer.

A non-zero exit code is not an error: it comes back in `exit_code`. The call
fails when the agent is not reachable, when the guest cannot start the program,
or when the timeout passes. Machines from templates built before the agent
existed have no exec transport and fail here.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else { return }
    match dc.exec("ipconfig", ["/all"]) {
        Ok(r) => { if r.exit_code == 0 { lab.log(r.stdout) } else { lab.log(r.stderr) } }
        Err(e) => lab.log("exec failed: " + e),
    }
}
```

## Machine.exec_timeout

`Machine.exec` with an explicit timeout.

```wscript
fn exec_timeout(self, cmd: string, args: List[string], timeout_secs: int) -> Result[ExecResult, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `cmd` | string | The program to run. |
| `args` | List[string] | Its arguments. |
| `timeout_secs` | int | How long the command may run, in seconds. Values below 1 are treated as 1. |

Identical to `Machine.exec` otherwise.

```wscript
fn main(lab: Lab) {
    let Ok(box) = lab.vm("box") else { return }
    let r = box.exec_timeout("apt-get", ["-y", "upgrade"], 1800)
}
```

## Machine.copy_to

Copy a host file into the guest.

```wscript
fn copy_to(self, local: string, guest_path: string) -> Result[unit, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `local` | string | The host file. Relative paths resolve against the script's directory. |
| `guest_path` | string | Where to write it in the guest. |

Runs over the agent's file session under the handle's identity, so a handle
from `Machine.as_login` writes into that login's home even before the account
has ever logged on. The transfer is verified by digest. It fails when the agent
is not reachable, when the host file cannot be read, or when the guest refuses
the write.

```wscript
fn main(lab: Lab) {
    let Ok(m) = lab.machine("api") else { return }
    let up = m.copy_to("conf/app.conf", "/etc/app.conf")
}
```

## Machine.copy_from

Copy a guest file to the host.

```wscript
fn copy_from(self, guest_path: string, local: string) -> Result[unit, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `guest_path` | string | The file in the guest. |
| `local` | string | Where to write it on the host. Relative paths resolve against the script's directory. |

The mirror of `Machine.copy_to`. Missing parent directories on the host are
created. It fails when the agent is not reachable, when the guest file cannot
be read under the handle's identity, or when the host write fails.

```wscript
fn handle(event: Event, lab: Lab) {
    if event.name != "container.crashed" { return }
    let Ok(m) = lab.machine(event.vm) else { return }
    let down = m.copy_from("/var/log/app.log", "artefacts/app.log")
}
```

## Machine.logs

The last lines of the machine's console log.

```wscript
fn logs(self, lines: int) -> Result[string, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `lines` | int | How many lines from the end to return. A negative value is treated as 0. |

For a container this is its captured stdout and stderr with the micro-VM kernel
log; for a VM it is the serial log. It fails with "machine <name> has no
console log" where the machine keeps none, and with the read error otherwise.

```wscript
fn main(lab: Lab) {
    let Ok(web) = lab.container("web") else { return }
    match web.logs(50) {
        Ok(text) => lab.log(text),
        Err(e) => lab.log(e),
    }
}
```

## Machine.terminal

Open an interactive shell in the guest, driven send/expect style.

```wscript
fn terminal(self) -> Result[Term, string]
```

Opens one agent terminal session, 120 columns by 32 rows, running as the
handle's identity, and returns a `Term` handle over it. The shell sees a real
PTY, so prompts, echoes and escape sequences all arrive in the output buffer.
Every call opens a new shell. Close it with `Term.close`. It fails when the
agent is not reachable or lacks the terminal feature.

```wscript
fn main(lab: Lab) {
    let Ok(vm) = lab.vm("box") else { return }
    let Ok(t) = vm.terminal() else { return }
    let s = t.send_line("hostname")
    let out = t.expect("box", 10)
    t.close()
}
```

## Machine.logins

The logins the lab file declares for this machine.

```wscript
fn logins(self) -> List[Login]
```

Returns one `Login` per `login {}` block, with the two implicit rules applied: a
lone login is the default, and an undeclared `elevated` is true on Windows and
false on Linux. The password crosses exactly as written, `None` where none was
declared, so a provision creates the account the lab declares rather than a copy
of it. It never fails; a machine with no logins returns an empty list. See
logins-and-ssh.md.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else { return }
    for login in dc.logins() {
        let Some(password) = login.password else { continue }
        let r = dc.exec("net", ["user", login.user, password, "/add"])
    }
}
```

## Machine.as_login

A second handle onto the same machine whose guest work runs as a declared
login.

```wscript
fn as_login(self, selector: string) -> Result[Machine, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `selector` | string | A `login {}` label, the account name as an alias, or the family floor `SYSTEM` or `root`, which resolves back to the agent identity. |

The identity is resolved once, here, through the same resolution `vmlab exec
--user` uses, so a script and the CLI cannot disagree about what a label names.
Every method on the returned handle, `exec`, `copy_to`, `copy_from`,
`terminal`, lands under that identity, which is how a `provision {}` step
writes into the dev login's home before that user has ever logged on. The
pointer position is shared with the original handle. It fails, loudly, when
nothing matches the selector; that is what stops a provision from silently
writing into the system profile. See logins-and-ssh.md.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else { return }
    let Ok(dev) = dc.as_login("dev") else { return }
    let c = dev.copy_to("settings.json", "%APPDATA%\\Code\\User\\settings.json")
}
```

## Machine.as_account

A second handle whose guest work runs as an account the lab file does not
declare.

```wscript
fn as_account(self, user: string, password: string) -> Result[Machine, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `user` | string | The guest account, for example `PROBE\audit`. |
| `password` | string | Its secret, or a rotated one for a declared account. |

The same as `Machine.as_login` with the secret supplied alongside, the pair
`vmlab exec --user --password` takes. It fails when the guest rejects the pair
or when the machine's guest family cannot mint a logon for it.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else { return }
    let Ok(audit) = dc.as_account("PROBE\\audit", "s3cret") else { return }
    let r = audit.exec("whoami", [])
}
```

## Machine.stats

One sample of the guest's CPU, memory and disk usage.

```wscript
fn stats(self) -> Result[GuestStats, string]
```

Asks the agent's metrics feature, waiting up to ten seconds, and returns a
`GuestStats` record. It fails when the agent is not reachable, lacks the
metrics feature, or does not answer in time.

```wscript
fn main(lab: Lab) {
    let Ok(m) = lab.machine("api") else { return }
    match m.stats() {
        Ok(s) => { for d in s.disks { lab.log(d.mount) } }
        Err(e) => lab.log(e),
    }
}
```
