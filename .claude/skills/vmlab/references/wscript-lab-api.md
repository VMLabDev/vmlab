# wscript API: Lab, Segment, Term and the shared types

Method-by-method reference for the two handles a script starts from — `Lab`,
which every script receives, and `Segment`, which a lab handle gives out by name
— plus `Term`, the six record types methods return or handlers receive, and the
two functions on the `vmlab` module. `Machine` is in wscript-machine-api.md; the
language itself is in wscript-language.md.

Every script begins with `use vmlab`. A provision script or an ad-hoc
`vmlab script` run defines `fn main(lab: Lab)`; an event handler defines
`fn handle(event: Event, lab: Lab)`. Both handles are opaque: they carry no
fields, only methods. Every method that can fail returns a wscript `Result` whose
error is a string, and an error that propagates out of `main` fails the provision
run and therefore `vmlab up`. The signatures below use the parameter names the
host registers; the generated `vmlab.wscripti` interface file spells them `a0`,
`a1` and so on.

The record types are plain structs: a script reads their fields directly, as
`r.exit_code` or `login.password`. They are never constructed by a script.

`Vm` is a silent alias for `Machine`, kept so first-boot scripts sealed into
older templates keep compiling. New scripts write `Machine`.

## Lab.name

The lab's name, as the lab file declares it.

```wscript
fn name(self) -> string
```

Returns the name from the `lab "<name>" {}` block. It never fails.

```wscript
fn main(lab: Lab) {
    lab.log("provisioning lab " + lab.name())
}
```

## Lab.log

Write one line to the lab log and to the terminal of the CLI that started the
run.

```wscript
fn log(self, msg: string)
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `msg` | string | The text to write. A newline is appended. |

The line goes to `lab.log` under the lab's state directory and is streamed live
to the `vmlab up` or `vmlab script` invocation that is running the script. From
an event handler it lands in the lab log only. It never fails.

```wscript
fn main(lab: Lab) {
    for m in lab.machines() {
        lab.log(m.name() + ": " + m.state())
    }
}
```

## Lab.machine

The handle for one machine of either kind, by name.

```wscript
fn machine(self, name: string) -> Result[Machine, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `name` | string | The name of a `vm {}` or `container {}` block in the lab file. |

Returns a `Machine` handle (wscript-machine-api.md) running as the agent
identity. The error names the machine when the lab declares none by that name.
Use this when the script does not care which kind it is talking to; an event
handler that reads `event.vm` usually does not.

```wscript
fn handle(event: Event, lab: Lab) {
    if event.name == "container.crashed" {
        let Ok(m) = lab.machine(event.vm) else { return }
        let started = m.start()
    }
}
```

## Lab.machines

Every machine in the lab, VMs and containers alike.

```wscript
fn machines(self) -> List[Machine]
```

Returns one handle per declared machine. The order is the runtime's own and is
not the declaration order. It never fails; an empty lab returns an empty list.

```wscript
fn main(lab: Lab) {
    for m in lab.machines() {
        let r = m.wait_ready(300)
    }
}
```

## Lab.vm

The handle for one VM, by name, refusing a container.

```wscript
fn vm(self, name: string) -> Result[Machine, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `name` | string | The name of a `vm {}` block. |

The same handle `Lab.machine` returns, with a kind check in front. A name the lab
does not declare fails as `Lab.machine` does. A name that is a container fails
with a message saying so and pointing at `lab.container()`, which tells you more
than "no such machine" would. Every operation on the returned handle is available
on containers too; the check exists only so a script that knows what it declared
reads well.

```wscript
fn main(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else {
        lab.log("no dc01")
        return
    }
    let r = dc.wait_ready(600)
}
```

## Lab.this_vm

The machine whose provision block declared the running script.

```wscript
fn this_vm(self) -> Result[Machine, string]
```

Set for a `provision {}` block declared inside a `vm {}` or `container {}`, and
for a template's first-boot script, where it is the build VM. It fails with a
message saying so from an event handler and from `vmlab script`, because neither
has an owning machine.

Inside a template first-boot script the handle it returns is gated: on that
handle, `is_ready` and `wait_ready` mean "the agent answers" rather than full
readiness, because full readiness is unreachable until the script itself returns.

```wscript
fn main(lab: Lab) {
    let vm = lab.this_vm().expect("no target vm")
    match vm.exec("hostname", []) {
        Ok(r) => lab.log("running on " + r.stdout),
        Err(e) => lab.log("exec failed: " + e),
    }
}
```

## Lab.vms

The VMs of the lab, and only the VMs.

```wscript
fn vms(self) -> List[Machine]
```

`Lab.machines` filtered to machines whose kind is `vm`. It never fails.

```wscript
fn main(lab: Lab) {
    for vm in lab.vms() {
        let s = vm.snapshot("baseline")
    }
}
```

## Lab.container

The handle for one container, by name, refusing a VM.

```wscript
fn container(self, name: string) -> Result[Machine, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `name` | string | The name of a `container {}` block. |

The mirror of `Lab.vm`: the same handle, with the opposite kind check. A VM's
name fails with a message pointing at `lab.vm()`. See containers.md for what a
container can and cannot do at call time.

```wscript
fn main(lab: Lab) {
    let Ok(web) = lab.container("web") else { return }
    match web.logs(50) {
        Ok(text) => lab.log(text),
        Err(e) => lab.log(e),
    }
}
```

## Lab.containers

The containers of the lab, and only the containers.

```wscript
fn containers(self) -> List[Machine]
```

`Lab.machines` filtered to machines whose kind is `container`. It never fails.

```wscript
fn main(lab: Lab) {
    for c in lab.containers() {
        if !c.is_healthy() { lab.log(c.name() + " is unhealthy") }
    }
}
```

## Lab.segment

The handle for one segment, by name.

```wscript
fn segment(self, name: string) -> Result[Segment, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `name` | string | The name of a `segment {}` block in the lab file. |

The error names the segment and the lab when no segment of that name is
assembled. A handle is a name, not a lock: every method on it looks the segment
up again, so a segment torn down after the handle was taken fails on the next
call with "segment is gone".

```wscript
fn main(lab: Lab) {
    let Ok(lan) = lab.segment("lan") else { return }
    let id = lan.dns_set("intranet.corp.local", "10.0.0.10")
}
```

## Segment.name

The segment's name.

```wscript
fn name(self) -> string
```

Returns the name the handle was taken with. It never fails.

```wscript
fn main(lab: Lab) {
    let Ok(seg) = lab.segment("lan") else { return }
    lab.log("rules on " + seg.name())
}
```

## Segment.dns_set

Add a static DNS record, or a wildcard, to the segment's zone.

```wscript
fn dns_set(self, name: string, ip: string) -> Result[int, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `name` | string | An exact name, or a `*.`-prefixed wildcard pattern. Taken verbatim; the segment's DNS suffix is not appended. |
| `ip` | string | The IPv4 address to answer with. |

Returns a rule id for `Segment.dns_clear`. An exact name becomes a static record
that replaces any earlier record for that name; a pattern beginning with `*.`
becomes a wildcard. Lookup precedence in the zone is sinkhole, then exact record,
then wildcard, then the upstream forwarder, else NXDOMAIN.

It fails when `ip` does not parse as an IPv4 address, when the segment is gone,
or when the segment has DNS disabled.

```wscript
fn main(lab: Lab) {
    let Ok(lan) = lab.segment("lan") else { return }
    match lan.dns_set("*.apps.corp.local", "10.0.0.20") {
        Ok(id) => lab.log("wildcard rule " + id),
        Err(e) => lab.log(e),
    }
}
```

## Segment.dns_sinkhole

Answer NXDOMAIN for every name matching a pattern.

```wscript
fn dns_sinkhole(self, pattern: string) -> Result[int, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `pattern` | string | The name or `*.`-wildcard to sink. |

Returns a rule id for `Segment.dns_clear`. Sinkholes win over every other kind of
record; among sinkholes the most specific pattern wins and declaration order
breaks ties. The script surface always sinks with NXDOMAIN; the other sinkhole
modes are available in the lab file only (lab-file.md). It fails on a gone
segment or one with DNS disabled.

```wscript
fn main(lab: Lab) {
    let Ok(lan) = lab.segment("lan") else { return }
    let id = lan.dns_sinkhole("*.telemetry.example.com")
}
```

## Segment.dns_clear

Remove a rule added by `Segment.dns_set` or `Segment.dns_sinkhole`.

```wscript
fn dns_clear(self, rule_id: int) -> Result[bool, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `rule_id` | int | The id an earlier `dns_set` or `dns_sinkhole` returned. |

Returns `true` when something was removed and `false` when the id matched
nothing. Records the lab file declared have no id and cannot be cleared from a
script. It fails on a gone segment or one with DNS disabled.

```wscript
fn main(lab: Lab) {
    let Ok(lan) = lab.segment("lan") else { return }
    let Ok(id) = lan.dns_sinkhole("updates.example.com") else { return }
    let removed = lan.dns_clear(id)
}
```

## Segment.block

Drop every packet to a destination address or range.

```wscript
fn block(self, cidr: string) -> Result[int, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `cidr` | string | An IPv4 network in CIDR form, or a single IPv4 address. |

Returns a rule id for `Segment.unblock`. The rule applies to every protocol and
port. It fails when `cidr` is neither a network nor an address, when the segment
is gone, or when the segment runs no network services, which is the case for a
global segment the supervisor gateways.

```wscript
fn main(lab: Lab) {
    let Ok(lan) = lab.segment("lan") else { return }
    match lan.block("203.0.113.0/24") {
        Ok(id) => lab.log("block rule " + id),
        Err(e) => lab.log(e),
    }
}
```

## Segment.block_port

Drop packets to a destination for one protocol and port.

```wscript
fn block_port(self, cidr: string, proto: string, port: int) -> Result[int, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `cidr` | string | An IPv4 network in CIDR form, or a single address. |
| `proto` | string | `tcp`, `udp` or `icmp`. |
| `port` | int | The destination port, 0 to 65535. |

Returns a rule id for `Segment.unblock`. It fails on a malformed `cidr`, an
unknown `proto`, a port out of range, a gone segment, or a segment without
network services.

```wscript
fn main(lab: Lab) {
    let Ok(lan) = lab.segment("lan") else { return }
    let id = lan.block_port("0.0.0.0/0", "tcp", 25)
}
```

## Segment.unblock

Remove a block or redirect rule by id.

```wscript
fn unblock(self, rule_id: int) -> Result[bool, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `rule_id` | int | The id `block`, `block_port` or `redirect` returned. |

Returns `true` when a rule was removed and `false` when nothing carried that id.
Despite the name it removes redirects too; the block and redirect tables share
one id space. It fails on a gone segment or one without network services.

```wscript
fn main(lab: Lab) {
    let Ok(lan) = lab.segment("lan") else { return }
    let Ok(id) = lan.block("198.51.100.7") else { return }
    vmlab::sleep_ms(30000)
    let gone = lan.unblock(id)
}
```

## Segment.redirect

Rewrite the destination of packets from one address to another.

```wscript
fn redirect(self, from: string, to: string) -> Result[int, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `from` | string | The original destination, `ip` or `ip:port`. |
| `to` | string | The new destination, `ip` or `ip:port`. |

Returns a rule id that `Segment.unblock` removes. Redirects are evaluated before
blocks. The script surface leaves the protocol unset, so the rule matches every
protocol; a protocol-specific redirect is declared in the lab file. It fails when
either endpoint has a malformed IP or port, or on a gone segment or one without
network services.

```wscript
fn main(lab: Lab) {
    let Ok(lan) = lab.segment("lan") else { return }
    let id = lan.redirect("8.8.8.8:53", "10.0.0.1:53")
}
```

## Segment.forward

Publish a guest TCP port on the host.

```wscript
fn forward(self, host_port: int, vm: string, guest_port: int) -> Result[int, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `host_port` | int | The host port to listen on, bound on every host address. |
| `vm` | string | The machine whose port to reach, by name. |
| `guest_port` | int | The port inside that machine. |

Returns the forward's id. The forward is TCP only and is resolved once: the
machine's first agent-reported IPv4 address at the time of the call, so the
machine must be up and leased. It fails when either port is out of range, when
the machine does not exist, when the agent reports no IPv4 address yet, when the
segment has no NAT (a forward needs egress to originate the guest-side
connection), or on a gone segment or one without network services. Forwards the
lab file declares are computed up front instead; see networking.md.

```wscript
fn main(lab: Lab) {
    let Ok(lan) = lab.segment("lan") else { return }
    let Ok(web) = lab.container("web") else { return }
    let r = web.wait_ready(120)
    let id = lan.forward(8080, "web", 80)
}
```

## Segment.route_to

Opt this segment into routing to another. Not available from scripts in this
release.

```wscript
fn route_to(self, other: string) -> Result[unit, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `other` | string | The segment to route to. |

The method compiles and always fails with "inter-segment routing is not yet
available from scripts". Declare routing in the lab file instead; see
networking.md.

```wscript
fn main(lab: Lab) {
    let Ok(lan) = lab.segment("lan") else { return }
    match lan.route_to("dmz") {
        Ok(_) => lab.log("routed"),
        Err(e) => lab.log(e),
    }
}
```

## Segment.unroute_to

Reverse `Segment.route_to`. Not available from scripts in this release.

```wscript
fn unroute_to(self, other: string) -> Result[unit, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `other` | string | The segment to stop routing to. |

Always fails with the same message as `Segment.route_to`.

```wscript
fn main(lab: Lab) {
    let Ok(lan) = lab.segment("lan") else { return }
    let r = lan.unroute_to("dmz")
}
```

## Segment.rules

The segment's live block and redirect rules, as JSON text.

```wscript
fn rules(self) -> Result[string, string]
```

Returns a JSON array in evaluation order: redirects first, then blocks, each in
insertion order. Every element carries `id`, `kind` (`redirect` or `block`) and a
one-line `description`. A block also carries `cidr`, and `port` when it has one;
a redirect carries `from` and `to`; either carries `proto` when it is
protocol-specific. Rules from the lab file and rules added by script appear
together. DNS rules are not included. It fails on a gone segment or one without
network services.

```wscript
fn main(lab: Lab) {
    let Ok(lan) = lab.segment("lan") else { return }
    match lan.rules() {
        Ok(json) => lab.log(json),
        Err(e) => lab.log(e),
    }
}
```

## Term

`Term` is the send/expect handle `Machine.terminal()` returns
(wscript-machine-api.md). It is an interactive shell session: the shell sees a
real PTY, so prompts, echoes and ANSI sequences are all in the buffer.

```ws
let t = vm.terminal()?
let s = t.send_line("hostname")
let out = t.expect("box", 10)?
t.close()
```

### Term.send

Send raw text to the shell.

```wscript
fn send(self, text: string) -> Result[unit, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `text` | string | The bytes to write to the PTY, exactly as given. No newline is added. |

Use this for control characters and partial input; the example below sends Ctrl-C
as the byte 0x03. It fails with "<machine>: terminal session is closed" after
`Term.close` or after the shell has exited, and with the agent's error if the
write fails.

```wscript
fn main(lab: Lab) {
    let Ok(vm) = lab.vm("box") else { return }
    let Ok(t) = vm.terminal() else { return }
    let s = t.send("\u{3}")
    t.close()
}
```

### Term.send_line

Send a line of input followed by Enter.

```wscript
fn send_line(self, text: string) -> Result[unit, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `text` | string | The line to type. |

Writes `text` and a carriage return, which is what a PTY expects from Enter and
works for POSIX shells and PowerShell alike. The errors are those of `Term.send`.

```wscript
fn main(lab: Lab) {
    let Ok(vm) = lab.vm("box") else { return }
    let Ok(t) = vm.terminal() else { return }
    let s = t.send_line("systemctl restart app")
    t.close()
}
```

### Term.read

Take whatever output the shell has produced so far.

```wscript
fn read(self) -> string
```

Drains output already queued, waiting at most about 50 milliseconds for more, and
returns it, clearing the buffer. It does not wait for a prompt; use `Term.expect`
for that. It never fails: a closed session returns what was left in the buffer,
or an empty string.

```wscript
fn main(lab: Lab) {
    let Ok(vm) = lab.vm("box") else { return }
    let Ok(t) = vm.terminal() else { return }
    vmlab::sleep_ms(500)
    lab.log(t.read())
    t.close()
}
```

### Term.expect

Wait until the output matches a regular expression and return the text up to the
end of the match.

```wscript
fn expect(self, pattern: string, timeout_secs: int) -> Result[string, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `pattern` | string | A regular expression, matched anywhere in the accumulated output. |
| `timeout_secs` | int | How long to wait, in seconds. |

Output accumulates in a buffer. On a match the text through the end of the match
is consumed and returned, and what follows stays for the next call, so successive
expects walk the stream. The buffer holds the raw PTY stream, prompts and escape
sequences included, so match on something stable. It fails with
"bad pattern: <reason>" for an invalid expression, with "<machine>: timed out
after <n>s waiting for /<pattern>/; tail: <text>" at the deadline, and with
"<machine>: terminal ended (<why>) before /<pattern>/ matched; tail: <text>" when
the shell exits or the agent channel closes first. The tail is the last few
hundred bytes of unmatched output, for debugging.

```wscript
fn main(lab: Lab) {
    let Ok(vm) = lab.vm("box") else { return }
    let Ok(t) = vm.terminal() else { return }
    let s = t.send_line("hostname")
    match t.expect("box", 10) {
        Ok(out) => lab.log("saw: " + out),
        Err(e) => lab.log(e),
    }
    t.close()
}
```

### Term.resize

Resize the session's PTY.

```wscript
fn resize(self, cols: int, rows: int) -> Result[unit, string]
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `cols` | int | The new width in columns. Clamped to at least 2. |
| `rows` | int | The new height in rows. Clamped to at least 2. |

A session opens at 120 by 32, which is wide enough that prompts rarely wrap
mid-pattern. It fails on a closed session and with the agent's error otherwise.

```wscript
fn main(lab: Lab) {
    let Ok(vm) = lab.vm("box") else { return }
    let Ok(t) = vm.terminal() else { return }
    let rz = t.resize(200, 50)
    t.close()
}
```

### Term.close

End the session and kill the shell.

```wscript
fn close(self)
```

Closing is deterministic; a handle that goes out of scope without it is closed
when the script's VM collects it. Closing twice is harmless. It never fails.
After it, `Term.send`, `Term.send_line` and `Term.resize` fail with "terminal
session is closed" and `Term.expect` reports the session ended with reason
"closed".

```wscript
fn main(lab: Lab) {
    let Ok(vm) = lab.vm("box") else { return }
    let Ok(t) = vm.terminal() else { return }
    t.close()
}
```

## Match

Where an image or text was found on the display, returned by the screen-matching
methods.

```wscript
struct Match {
    x: int,
    y: int,
    w: int,
    h: int,
    score: float,
    cx: int,
    cy: int,
    text: string,
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `x` | int | 0 | Left edge of the match, in framebuffer pixels. |
| `y` | int | 0 | Top edge of the match. |
| `w` | int | 0 | Width of the matched region; the reference image's width. |
| `h` | int | 0 | Height of the matched region. |
| `score` | float | 1.0 | The similarity score, 0 to 1. |
| `cx` | int | 0 | Horizontal centre, for `Machine.mouse_move`. |
| `cy` | int | 0 | Vertical centre. |
| `text` | string | empty | The matched text, set only by `Machine.wait_for_text`. |

From `wait_for_image`, `wait_for_image_opts`, `wait_for_any` and `find_image` the
position fields describe the match and `text` is empty. From `wait_for_text` only
`text` is meaningful: the position fields are 0 and the score is 1, because OCR
does not report a location. The defaults column shows those filler values.

## ExecResult

What a guest command produced, returned by `Machine.exec` and
`Machine.exec_timeout`.

```wscript
struct ExecResult {
    exit_code: int,
    stdout: string,
    stderr: string,
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `exit_code` | int | none | The process's exit code. Non-zero is not an error to the call. |
| `stdout` | string | none | Captured standard output, decoded as UTF-8 with invalid bytes replaced. |
| `stderr` | string | none | Captured standard error, decoded the same way. |

Every field is always set. Branch on `exit_code` for the command's own verdict;
the call's `Result` reports only whether the command could be run at all. The
command is an argv, not a shell line, so a pipeline needs an explicit
`"/bin/sh", ["-c", "…"]` or `"cmd.exe", ["/c", "…"]`. Output is streamed and
captured, not polled. A guest built from a template with no agent has no exec
transport at all.

## Login

One identity the lab file declares for a machine, returned by `Machine.logins`.

```wscript
struct Login {
    label: string,
    user: string,
    password: Option[string],
    elevated: bool,
    default: bool,
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `label` | string | none | The name an SSH username or `--user` selects this identity by. |
| `user` | string | none | The guest account, for example `PROBE\dev`. |
| `password` | Option[string] | `None` | The declared secret exactly as written, or `None` where the author declared none. Never an empty string. |
| `elevated` | bool | true on Windows, false on Linux | Whether sessions as this login run elevated. Resolved: an undeclared value takes the guest family's default. Elevation is Windows-only, so a Linux login that never declared it reports false. |
| `default` | bool | resolved | Whether this is the machine's default identity, with the lone-login rule applied: a machine's only login is the default without saying so. |

`password` is `None` rather than empty so that a script never passes an empty
secret to an account-creation command by accident. `elevated` and `default` cross
resolved rather than as written, so a script asking "is this the default" gets
the answer vmlab acts on. See logins-and-ssh.md and the `login {}` block in
vm.md.

## GuestStats

One sample of guest metrics, returned by `Machine.stats`.

```wscript
struct GuestStats {
    cpu_pct: float,
    mem_used: int,
    mem_total: int,
    disks: List[DiskStat],
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `cpu_pct` | float | none | CPU use across the guest, in percent. |
| `mem_used` | int | none | Memory in use, in bytes. |
| `mem_total` | int | none | Memory the guest sees, in bytes. |
| `disks` | List[DiskStat] | empty | One entry per mounted filesystem. |

The sample comes from the agent's metrics feature; `vmlab machine capabilities`
says whether a machine's agent offers it.

## DiskStat

One mounted filesystem inside `GuestStats`.

```wscript
struct DiskStat {
    mount: string,
    used: int,
    total: int,
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `mount` | string | none | The mount point, such as `/` or `C:\`. |
| `used` | int | none | Bytes in use. |
| `total` | int | none | The filesystem's size in bytes. |

## Event

The payload an event handler receives as its first argument.

```wscript
struct Event {
    name: string,
    vm: string,
    data: string,
}

fn handle(event: Event, lab: Lab) { }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | string | none | The event name, such as `vm.crashed`. See automation.md. |
| `vm` | string | empty | The machine the event concerns: the payload's `vm` key, else its `container` key, else empty. |
| `data` | string | `{}` | The whole payload as JSON text. |

A handler is bound with an `on {}` block in the lab file (automation.md) and runs
`fn handle(event: Event, lab: Lab)` on a lab handle with no owning machine.
Handler failures are logged and never fatal. `vm` is filled from the `vm` or
`container` key only, so an event that names its subject as `machine` alone
leaves it empty; the daemons set both keys on such events for that reason. `data`
carries every other field for a handler to parse.

```wscript
fn handle(event: Event, lab: Lab) {
    lab.log("event " + event.name + " on " + event.vm)
    if event.name == "vm.crashed" {
        let Ok(m) = lab.machine(event.vm) else { return }
        let shot = m.screenshot("")
    }
}
```

## vmlab::sleep_ms

Pause the script.

```wscript
fn sleep_ms(ms: int)
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `ms` | int | How long to sleep, in milliseconds. A negative value returns at once. |

Blocks the script's thread; the lab daemon keeps running. Prefer the waiting
methods, `Machine.wait_ready`, `Machine.wait_for_image`, `Term.expect`, where one
fits, and use this for polling loops around `Machine.exec`.

```wscript
fn main(lab: Lab) {
    let vm = lab.this_vm().expect("no target vm")
    for i in 0..10 {
        match vm.exec("cmd.exe", ["/c", "if exist C:\\done (exit 0) else (exit 1)"]) {
            Ok(r) => { if r.exit_code == 0 { return } }
            Err(e) => lab.log("waiting: " + e),
        }
        vmlab::sleep_ms(1000)
    }
}
```

## vmlab::env

Read an environment variable of the lab daemon's process.

```wscript
fn env(name: string) -> string
```

| Parameter | Type | Meaning |
| --- | --- | --- |
| `name` | string | The variable's name. |

Returns the value, or an empty string when it is unset, so a script cannot tell
an unset variable from an empty one. The environment is the daemon's, which
inherits from the `vmlab` invocation that started it, not necessarily from the
shell running `vmlab up` now. It exists so a build or provision script can carry
an operator toggle without a schema change, such as a `VMLAB_SKIP_UPDATES=1` that
lets a template build skip its update pass.

```wscript
fn main(lab: Lab) {
    if vmlab::env("VMLAB_SKIP_UPDATES") == "1" {
        lab.log("skipping updates")
        return
    }
}
```
