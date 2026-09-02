# The wscript language

Every script vmlab runs is written in **wscript**, a small statically typed
language with Rust-shaped syntax. vmlab registers its lab, machine and segment
API as a wscript host module, so a provision script, an event handler and an
ad-hoc `vmlab script` run all see the same surface. Scripts are daemon-unaware:
they receive a `Lab` handle and operate on it, and every blocking call takes a
timeout and returns a `Result`.

Signatures: see wscript-lab-api.md (`Lab`, `Segment`, `Term`, record types, the
`vmlab` module) and wscript-machine-api.md (`Machine`).

## The language in brief

wscript is statically typed with local inference: annotate function signatures
and nothing else. A type error, including a wrong argument to a vmlab method, is
reported when the script compiles rather than when it runs. `vmlab validate`
compiles every script the lab file references, so a typo in a method name fails
before any machine boots (see lab-file.md).

Types met most: `string`, `int`, `float`, `bool`, `unit`, `List[T]`, `Option[T]`,
`Result[T, E]`.

Statements end at newlines; no semicolons.

`if` and `match` are expressions.

Loops: `for x in xs`, `for i in 0..n`, `while`, `loop`.

`fmt("{} ready", name)` builds a string.

String methods: `trim`, `split`, `contains`, `to_lower`, `ends_with`.

## Modules and the `vmlab` module

A script names the host module with `use vmlab` at the top. That brings the
`Lab`, `Machine`, `Segment` and `Term` types into scope, along with the `vmlab::`
module functions:

- `vmlab::sleep_ms(ms)` pauses the script.
- `vmlab::env(name)` reads a host environment variable, returning an empty string
  when it is unset.

Both are specified in wscript-lab-api.md.

The older name `Vm` still compiles as a silent alias for `Machine`, kept so
first-boot scripts sealed into earlier templates keep compiling. New scripts
write `Machine`.

## The host module and its three handles

The module exposes three opaque handle types — `Lab`, `Machine`, `Segment`. They
carry no fields, only methods. All state rides inside them, so the same module
serves compile-checking and live execution.

`Lab` is what every entry point receives. It answers `name()`, writes to the lab
log and the live CLI stream with `log(msg)`, and hands out the other two handles.
`lab.machine(name)` returns any machine; `lab.vm(name)` and `lab.container(name)`
return the same handle but refuse the other kind with a message that says which
it is, which is a better error than "no such machine". `machines()`, `vms()` and
`containers()` list them, and `lab.segment(name)` returns a segment handle or an
error naming the lab.

There is one `Machine` handle for a VM and a container alike. Every operation is
available on every machine, and what a particular machine cannot do is reported
at call time by naming the capability: `screenshot` on a machine with no display
fails with "machine `api` has no display", never with "no such method" and never
with a claim about its kind. Its methods fall into six groups: lifecycle and
state, snapshots, input and screen (see snapshots-vision.md), the guest agent,
and identity (see logins-and-ssh.md). `poweroff` is a clean QMP quit that flushes
block caches first, unlike `stop_force`, which kills QEMU; for a guest with no
ACPI it is the only way to seal a consistent disk. Snapshot calls go through the
lab runtime so the records, events and pin-guarding stay in one place; see
snapshots-vision.md for what a restore does to a dev machine's workspace. Full
signatures in wscript-machine-api.md.

`Segment` mutates a segment's network services at runtime: `dns_set`,
`dns_sinkhole` and `dns_clear` edit the segment's DNS zone, `block`,
`block_port`, `unblock` and `redirect` edit its L3 rules, `forward` adds a host
port forward to a machine's leased address, and `rules()` returns the current
rule set as JSON. Each rule-adding call returns a rule id the removing call takes
back. `route_to` and `unroute_to` exist in the interface but currently return an
error saying inter-segment routing is not yet available from scripts. See
networking.md for what these rules do.

## Result handling and pattern matching

Every call that can fail returns `Result[T, string]`, where the error is a
message naming the machine and the reason. Three ways to consume one:

- `?` propagates the error out of the current function. The function must itself
  return a `Result`, which is why most examples put their work in a
  `setup(lab) -> Result[unit, string]` and call it from `main`.
- `match r { Ok(v) => …, Err(e) => … }` handles both arms in place. `match` is
  exhaustive, so a missing arm is a compile error.
- `let Ok(vm) = lab.vm("dc01") else { return }` binds the success value or leaves
  the function. `let Some(x) = opt else { … }` does the same for an `Option`.

`r.expect("message")` and `r.unwrap()` take the value or raise a fault that ends
the script. The examples use `expect` in `main` to turn a failed setup into a
failed provision with a readable message.

A value you do not use must still be bound, so a fire-and-forget call reads
`let k = m.send_keys("enter")`.

```ws
// scripts/setup.ws
use vmlab

fn setup(lab: Lab) -> Result[unit, string] {
    let web = lab.vm("web")?
    web.wait_ready(600)?
    let r = web.exec_timeout("/bin/sh", ["-c", "apt-get install -y nginx"], 600)?
    if r.exit_code != 0 {
        return Err("nginx install failed: " + r.stderr)
    }
    lab.log("nginx installed on " + web.ip()?)
    Ok(())
}

fn main(lab: Lab) {
    setup(lab).expect("setup failed")
}
```

## Where a script runs from

A **provision script** is declared with a `provision "path.ws" { }` block inside
the `vm {}` or `container {}` it configures. Its entry point is
`fn main(lab: Lab)`. It runs once during `vmlab up`, after that machine is ready,
at its position among the machine's steps, interleaved with the machine's
playbooks (automation.md) in declaration order. Across machines, steps follow the
order the machine blocks appear, with `depends_on` gating when each becomes
eligible. A script orchestrating several machines is the normal case: it reaches
the others through the lab handle.

`lab.this_vm()` returns the machine whose `provision {}` block declared the
running script, or the build VM when a template's first-boot script runs. It is
an error from an event handler or from `vmlab script`, which have no owning
machine.

Inside a machine's **own first-boot provision**, `is_ready` and `wait_ready` mean
agent-level readiness rather than full readiness, because full readiness is
unreachable until that script returns; a first-boot script that reboots its guest
uses them to wait for it to come back. Everywhere else they mean full readiness.
`agent_answering` is the ungated live probe and goes false while a guest is down
or mid-reboot.

An **ad-hoc run** is `vmlab script scripts/whatever.ws`, given a path relative to
the lab root. It uses the same `main(lab)` entry point against the running lab,
with no owning machine. See cli-lab.md.

An **event handler** has the entry point `fn handle(event: Event, lab: Lab)` and
is bound with an `on {}` block. Handler failures are logged and never fatal. See
automation.md.

A **template build script** gets the same API scoped to the single build VM. See
templates.md.

Relative local paths in a script resolve against the script's own directory, not
the lab root: a `copy_to("scripts/editor-bits.ps1", …)` from
`scripts/editor-bits.ws` reads `scripts/scripts/editor-bits.ps1`. This is what
lets a provision ship reference images and payload files beside itself, and it
holds for template builds, which run from a separate working directory.

## The interface file and the LSP

vmlab ships its API as a `.wscripti` interface file so the wscript language
server can give diagnostics, hover and completion for lab scripts. The hidden
verb `vmlab __wscripti [path]` writes it, defaulting to `vmlab.wscripti` in the
current directory. List that file in a `wscript.toml` beside your scripts;
`wscript check` and `wscript lsp` read it from there. The file is generated and
says so in its header; do not edit it by hand.

The documented signatures use the parameter names the host registers; the
generated interface file spells them `a0`, `a1` and so on.

Note: a template stores the wscript surface version its scripts compiled against.
A template below this host's supported floor is refused with one message telling
you to rebuild it with this vmlab version, rather than a cascade of compiler
errors.

## Error handling

A compile error names the file, the message, the diagnostic code and any help
text.

A runtime error out of a provision script carries the message plus the call chain
it surfaced through, is written to the lab log as "script failed: …", and fails
the provision run, which fails `vmlab up`.

An error inside a handler is logged with a warning and nothing else stops.

A method error is always a string you can log or match on, and it names the
machine: a timed-out `wait_for_image` says how long it waited and on which
machine, and a `terminal` whose shell exited says so and quotes the tail of its
output.

Idempotence is the script author's job. Every `vmlab up` runs the provision steps
of each machine it brings into scope, whether that machine was just cloned or was
already running, so a script that must not repeat a step checks the guest first.
The mixed-lab example reads a registry value before enabling autologon and skips
the reboot when it is already set; see examples.md.

## Not in the language

- Inter-segment routing from scripts: `Segment.route_to` and
  `Segment.unroute_to` compile and always fail with "inter-segment routing is not
  yet available from scripts". Declare routing in the lab file.
- Sinkhole modes other than NXDOMAIN: the script surface always sinks with
  NXDOMAIN; the other modes are available in the lab file only.
- Protocol-specific redirects: the script surface leaves the protocol unset, so a
  scripted redirect matches every protocol. A protocol-specific redirect is
  declared in the lab file.
- Clearing DNS records the lab file declared: they have no id and cannot be
  cleared from a script.
- Constructing record types: `Match`, `ExecResult`, `Login`, `GuestStats`,
  `DiskStat` and `Event` are never constructed by a script; they are only read.
- Distinguishing an unset environment variable from an empty one: `vmlab::env`
  returns an empty string for both.
- An exec transport on a guest built from a template with no agent: there is
  none.
