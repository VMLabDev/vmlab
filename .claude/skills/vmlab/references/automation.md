# Automation: provisions, event handlers, playbooks

vmlab has three automation surfaces. Pick by job:

| Surface | Shape | Runs |
| --- | --- | --- |
| `provision "path.ws" {}` | imperative wscript, entry `fn main(lab: Lab)` | once during `vmlab up`, after the machine is ready, at its position among the machine's steps |
| `playbook "folder" {}` | declarative convergence, a config-weave play run inside the guest | during `vmlab up` at its position among the machine's steps, and on demand via `vmlab playbook check|apply` |
| `on "<event>" { run = "script.ws" }` | imperative wscript, entry `fn handle(event: Event, lab: Lab)` | whenever the bound event fires, in the lab daemon |

Use a playbook for what is done *to* the machine: packages, services, registry
settings, the toolchain. Use a provision for anything imperative, anything
orchestrating several machines, and anything that must land as a particular
login. Use a handler for reacting to something that happened.

## Provisions

A provision script is declared with a `provision "path.ws" { }` block inside
the `vm {}` or `container {}` it configures. Entry point `fn main(lab: Lab)`.
It runs once during `vmlab up`, after that machine is ready, at its position
among the machine's steps, interleaved with the machine's playbooks in
declaration order. Different machines provision in parallel, so a script cannot
assume another machine's steps have run. A script orchestrating several machines
is the normal case: it reaches the others through the lab handle, and its
machine declares the order it needs with `depends_on_provision`:

```wcl
vm "dc01" {
  template = "x86_64/windows-server-2025"
  provision "scripts/promote-dc.ws" { }
}

vm "ws01" {
  template             = "x86_64/windows-11"
  depends_on           = ["dc01"]   // boot once dc01 answers
  depends_on_provision = ["dc01"]   // join once dc01 is a controller
  provision "scripts/join-domain.ws" { }
}
```

Upgrading: `depends_on` used to hold a dependent until its dependency's
provisions had finished. It is now start order only; a lab that relied on the
old behaviour adds `depends_on_provision` naming the same machines.

`lab.this_vm()` returns the machine whose `provision {}` block declared the
running script, or the build VM when a template's first-boot script runs. It is
an error from an event handler or from `vmlab script`, which have no owning
machine.

Inside a machine's **own first-boot provision**, `is_ready` and `wait_ready`
mean agent-level readiness rather than full readiness, because full readiness
is unreachable until that script returns; a first-boot script that reboots its
guest uses them to wait for it to come back. Everywhere else they mean full
readiness. `agent_answering` is the ungated live probe and goes false while a
guest is down or mid-reboot, but it cannot see a reboot that finishes between
two probes. To watch a reboot the script asked for, read `boot_id` before
asking and call `wait_rebooted` after.

An **ad-hoc run** is `vmlab script scripts/whatever.ws`, given a path relative
to the lab root. It uses the same `main(lab)` entry point against the running
lab, with no owning machine.

A **template build script** gets the same API scoped to the single build VM.

Relative local paths in a script resolve against the script's own directory,
not the lab root: a `copy_to("scripts/setup.ps1", …)` from `scripts/join.ws`
reads `scripts/scripts/setup.ps1`. This is what lets a provision ship reference
images and payload files beside itself, and it holds for template builds, which
run from a separate working directory.

### Re-running and idempotence

Idempotence is the script author's job. Every `vmlab up` runs the provision
steps of each machine it brings into scope, whether that machine was just
cloned or was already running, so a script that must not repeat a step checks
the guest first. The mixed-lab example reads a registry value before enabling
autologon and skips the reboot when it is already set (see examples.md).

### Errors

A compile error names the file, the message, the diagnostic code and any help
text. `vmlab validate` compiles every script the lab file references, so a typo
in a method name fails before any machine boots.

A runtime error out of a provision script carries the message plus the call
chain it surfaced through, is written to the lab log as `script failed: …`, and
fails the provision run, which fails `vmlab up`. An error inside a handler is
logged with a warning and nothing else stops.

The language, the `Lab`/`Machine`/`Segment`/`Term` handles and the exec, copy
and terminal surfaces are in wscript-language.md, wscript-lab-api.md and
wscript-machine-api.md.

## Events and handlers

Every lab daemon emits structured events as things happen: a machine starting,
becoming ready, stopping or crashing; a lab coming up or down; a snapshot
taken; a playbook converging; the disk running low. Any unrecoverable error is
an event before it is a failure. Each event goes three places at once: the
daemon's live broadcast stream, which CLI subscribers and the supervisor's
host-wide aggregate read; the lab's append-only event history; and the tracing
log.

### Bindable events

An event is a name, the lab it came from, a timestamp and a JSON payload.
Machine events carry the machine's name under `vm`, or under `container` for a
container. The names a handler may bind are a closed list, and `vmlab validate`
rejects an `on {}` that names anything else:

| Event | When |
| --- | --- |
| `vm.starting`, `vm.ready`, `vm.stopped`, `vm.crashed` | A VM's lifecycle. `vm.stopped` follows every exit; `vm.crashed` precedes it when QEMU died rather than being asked to stop. |
| `vm.suspended`, `vm.woken` | A VM's guest put itself to sleep (ACPI S3), or runs again. |
| `container.starting`, `container.ready`, `container.stopped`, `container.crashed`, `container.unhealthy` | A container's lifecycle, plus its healthcheck turning unhealthy. See containers.md. |
| `lab.up`, `lab.down` | The end of an `up` and of a `down`, with the machines involved. |
| `lab.daemon_crashed` | Emitted by the supervisor when a lab daemon dies unexpectedly; the lab is marked failed and not restarted. |
| `snapshot.created`, `snapshot.restored` | A snapshot taken or restored on a machine. See snapshots-vision.md. |
| `template.built` | A template build sealed. See templates.md. |
| `usb.attached`, `usb.detached` | A host USB device hot-plugged into a VM, or taken out of it. See cli-machine.md (`vmlab usb`). |
| `record.started`, `record.stopped` | A console recording of a VM began, or was finalised to a file. See cli-machine.md (`vmlab record`). |
| `playbook.applied`, `playbook.failed` | A play converged, or a run ended non-zero or failed to run. |
| `host.disk_low` | The free-space watchdog crossed its threshold. |

Other events reach the stream and the history but cannot be bound: the workspace
syncer's `workspace.halted`, `workspace.synced`, `workspace.rescan`,
`workspace.deferred`, `workspace.skipped`, `workspace.refused`,
`workspace.volume` and their siblings, the per-step `playbook.op.*` progress,
and `smb.started` and `smb.failed` from the shared-folder service. They exist so
that a refusal or a skip is visible somewhere other than one developer's
terminal, and `vmlab logs` shows them.

### Writing a handler

An `on "<event>" { run = "<script.ws>" }` block at lab level binds the event to
a handler script. The script's entry point is `fn handle(event: Event, lab: Lab)`:
`event.name` is the event, `event.vm` is the machine's name for machine events
and empty otherwise, and `event.data` is the JSON payload as text. `lab` is the
same handle a provision gets, so a handler can screenshot the machine that
crashed, copy files off it, or start it again.

```wcl
# vmlab.wcl
lab "ad-lab" {
  // …
  on "vm.crashed"    { run = "scripts/collect-dumps.ws" }
  on "host.disk_low" { run = "scripts/alert.ws" }
}
```

```rust
// scripts/collect-dumps.ws
use vmlab

fn handle(event: Event, lab: Lab) {
    lab.log("crash handler fired for " + event.vm + " (" + event.name + ")")
    let Ok(vm) = lab.vm(event.vm) else { return }
    match vm.screenshot("") {
        Ok(path) => lab.log("saved crash screenshot: " + path),
        Err(e) => lab.log("could not screenshot: " + e),
    }
}
```

`targets = ["dc01", "web"]` narrows a handler to named machines; with no
`targets` it handles every occurrence. Only machine-scoped events accept
targets, the `vm.`, `container.` and `snapshot.` families, and `validate`
rejects targets on a lab-wide event such as `host.disk_low`. The handler's path
is relative to the lab root, must exist, and is compiled by `validate` along
with every other script.

The lab daemon subscribes to its own stream and, for each event, runs every
matching handler as its own task. Handlers run concurrently with each other and
with whatever caused the event, and shutdown waits a bounded time for in-flight
handlers rather than killing them. **Handler failures are logged, never fatal**:
a compile error, a runtime error or an unreadable script is a warning in the
daemon log and nothing else stops. There is no restart policy in the daemon; a
handler that wants one calls `machine.start()` itself, which is what the crash
handler in `examples/mixed-lab` demonstrates for a container.

A handler's `lab.log` output lands in the lab daemon's own process log,
`labd-<lab>.log` under `~/.local/state/vmlab/`, tagged with the `handler`
target, not on the CLI that happened to trigger the event.

### Watchdogs

Two free-space watchdogs run, both governed by `disk_low_percent` in the host
configuration (host-profiles.md), default 10 percent, and both checking once a
minute. The supervisor watches the filesystem holding the template store, since
pulls and builds land there. Each lab daemon watches the filesystem holding its
own `.vmlab/` directory, since linked clones grow with use. Each emits
`host.disk_low` with the path and the free percentage when free space drops
below the threshold, once, and re-arms only when space recovers above it, so a
full disk does not flood the stream. The lab daemon's copy is the one an
`on "host.disk_low"` handler sees.

The supervisor is also the watchdog over lab daemons. It reaps a daemon on
a full `down`, `lab stop` or `destroy`, and if one dies unexpectedly it emits `lab.daemon_crashed`,
marks the lab failed, and does not restart it.

### Logs

Everything is logged as JSON lines under `~/.local/state/vmlab/labs/<lab>/`: the
event history in `events.jsonl`, the lab log in `lab.log` with provision output,
and per machine a serial log, QEMU's stdout and stderr, and swtpm's log for a
VM, or the console log for a container. The files vmlab appends to itself roll
over at a fixed size, keeping one previous generation as `<name>.1`. Provision
output is also streamed live to the CLI that ran `up`.

`vmlab logs [lab/][machine]` dumps or follows them. With no target it takes the
current directory's lab, with a machine it narrows to that machine's files, `-f`
keeps following and picks up machines that start later, `-n` sets the history
shown, and `--output jsonl` emits the raw lines instead of the pretty rendering.
Event lines are rendered as a timestamp plus a flattened `event key=value …`
summary; every other stream passes through verbatim. Two related verbs read
inside the guest rather than about it: `vmlab tail` follows a file in the guest
over the agent, and `vmlab eventlog` follows a Windows guest's event log.

## Event catalogue

### Shape and delivery

An event is one JSON object with four fields: `event`, the name; `lab`, the lab
it belongs to, omitted for host-scoped events; `data`, the payload object,
omitted when there is none; and `ts`, a UTC timestamp. A lab daemon writes every
event it emits to three places: its tracing log, the lab's `events.jsonl` under
the state directory, and its broadcast stream, which the supervisor folds into a
host-wide aggregate. A subscriber attaches at either level. `vmlab logs` reads
the history file.

A handler script sees the machine an event concerns as `event.vm`, taken from
the payload's `vm` key, else its `container` key, else empty. Events whose
natural subject is `machine` set `vm` as well so a handler can look the machine
up.

```json
{"event":"vm.ready","lab":"demo","data":{"vm":"dc01"},"ts":"2026-09-02T10:14:03.512Z"}
```

### Lab lifecycle

| Event | Payload fields | When |
| --- | --- | --- |
| `lab.up` | `vms`: the names started | `vmlab up` finished, after provisioning and workspace start. |
| `lab.down` | none | `vmlab down` finished; also on the way through `destroy`. |
| `lab.daemon_crashed` | none; `lab` names the lab | Emitted by the supervisor when a lab daemon's socket is gone or unresponsive. The registry entry is marked failed. |
| `host.disk_low` | `path`, `free_percent` | Every sixty seconds while free space under the lab's `.vmlab/` is below the host config threshold. The supervisor emits the same event, with an empty `lab`, for the filesystem holding the template store. |

### Machines

VM events carry `vm`; container events carry `container`. `reason` on a stop is
`requested`, `guest_initiated` or `crashed`; `status` is the emulator's exit
status and `exit_code` the container's.

| Event | Payload fields | When |
| --- | --- | --- |
| `vm.starting` | `vm` | The VM's start began, after any deferred template download. |
| `vm.ready` | `vm` | The agent handshake completed, or an online snapshot finished loading, or the first-boot provision completed and was sealed. For a container in that last case the key is `container`. |
| `vm.crashed` | `vm`, `reason`, `status` | The emulator exited without being asked. Always followed by `vm.stopped`. |
| `vm.stopped` | `vm`, `reason`, `status` | The emulator exited for any reason. |
| `vm.suspended` | `vm`, `prevent_sleep` | The guest suspended itself to RAM (ACPI S3); the VM now reports `suspended`. `prevent_sleep = true` means a wake is already on its way. |
| `vm.woken` | `vm`, `cause`, `message` | A suspended guest runs again. `cause` is `prevent_sleep` (the daemon woke it the moment it slept), `start` (`vmlab vm start`), `stop` (a graceful stop woke it first), `restore` (an online snapshot restore), or `guest` (its own wake source, or a reset). |
| `vm.destroyed` | `vm` | `vmlab vm destroy` removed the VM's clones, run directory and workspace ledger. |
| `container.starting` | `container` | The container's start began, after the image pull. |
| `container.ready` | `container` | The micro-VM reported ready; port forwards are installed next. |
| `container.crashed` | `container`, `reason`, `exit_code` | The container exited without being asked. Always followed by `container.stopped`. |
| `container.stopped` | `container`, `reason`, `exit_code` | The container exited for any reason. |
| `container.unhealthy` | `container` | The container's healthcheck reported not healthy. |
| `container.destroyed` | `container` | `vmlab container destroy` removed everything the container materialised. |
| `machine.agent_repaired` | `vm`, `machine`, `agent_version` | `vmlab machine repair-agent` pushed the host's agent; the machine is now diverged from its template. |
| `machine.agent_updated` | `vm`, `machine`, `from`, `to`, `ok`, `error` | `vmlab up` or `vmlab vm start` found the VM's agent stamp differing from the shipped agent and pushed it. With `ok = true` the machine is now diverged; with `ok = false`, `error` says why the push failed and the old agent is still in place. `from` is null when the agent never answered. |
| `share.unmountable` | `vm`, `share`, `reason` | The mount plan holds a share this guest cannot mount, or a mount step still failed when its retries ran out (`share` is set then). See shares-media.md. |

### Snapshots

| Event | Payload fields | When |
| --- | --- | --- |
| `snapshot.created` | `vm`, `name`, `online` | A snapshot was taken, of a VM or a container alike, after the workspace pre-flight flush. |
| `machine.clock_drift` | `vm`, `machine`, `drift_ms`, `guest`, `host`, `corrected`, `error` | A fresh agent handshake found a `clock = "host"` guest more than 2 s from the host and set it. `drift_ms` is guest minus host; `corrected = false` carries the guest's refusal in `error`. |
| `machine.clock_unchecked` | `vm`, `machine`, `reason` | A `clock = "host"` machine's agent has no `get_time`, so its clock is not checked. Once per start. |
| `snapshot.restored` | `vm`, `name`, `online`, `clock` | A snapshot was restored, after the pin check and the syncer bracket. `clock` is present for an online restore only: `set` (set to the host's time), `kept` (the machine's `clock` is `snapshot`), `fixed` (a fixed date-time) or `not_set` (it should have been set and could not; the reason is in the daemon log and the CLI warning). |

Deleting a snapshot emits nothing.

### Worktree labs

See worktrees.md.

| Event | Payload fields | When |
| --- | --- | --- |
| `worktree.frozen` | `generation`, `snapshot`, `machines` | `vmlab worktree freeze` froze the main lab's machines as that generation, at that snapshot. |
| `worktree.thawed` | `dropped`: a list of `machine`, `name` | `vmlab worktree thaw` folded the freeze back into the main lab. `dropped` names each online snapshot `--drop-online-snapshots` gave up. |
| `machine.cloned_from_freeze` | `machine`, `generation`, `snapshot`, `main` | A worktree lab built the machine as a clone of that generation of the main checkout `main`, before its first start. |
| `lab.isolation` | `role`, `worktree`, `generation`, `snapshot`, `forward_bind`, `globals` | The lab daemon started under worktree isolation. `role` is `main` or `worktree`, `worktree` the linked worktree's name or null, `forward_bind` the address every forward binds, and `globals` each global segment with the name it `joined`. Emitted once per daemon start, before any machine starts. |

### Host USB devices

| Event | Payload fields | When |
| --- | --- | --- |
| `usb.attached` | `vm`, `port`, `id`, `product`, `qdev_id` | A host USB device was hot-plugged into a VM. `id` is `vid:pid`; `qdev_id` is `usbhost-<port>`. |
| `usb.detached` | `vm`, `port`, `id`, `product`, `qdev_id`, `reason` | A host USB device left a VM. `reason` is absent for a requested detach, `unplugged` when it left the host, `restore` when an online restore removed it first. |

A VM stop releases its devices with no `usb.detached` per device; `vm.stopped` says it.

### Console recordings

| Event | Payload fields | When |
| --- | --- | --- |
| `record.started` | `vm`, `path`, `options` | A console recording of the VM began (`vmlab record start`, `--record`, or a script's `record`). `options` is the set in force with defaults filled in: `fps`, `size`, and `compress_idle`, `timeline`, `max_duration` where asked for. |
| `record.stopped` | `vm`, `path`, `duration_secs`, `reason`; or `vm`, `path`, `error` | The recording was finalised, `duration_secs` of video long. `reason` is `stopped` (a `record stop` or a handle's `stop`), `script run ended`, `machine stopped`, `machine destroyed`, `lab destroyed`, `disk space low`, or what ended it on its own (its maximum duration, an encoder failure). When ffmpeg failed while finalising, `error` carries why instead. |

### Downloads

Deferred downloads emit under two prefixes: `template.pull` for a VM's disk
image and `container.pull` for a container image. The subject key is `vm` or
`container` to match, and one event is emitted per machine waiting on the
download.

| Event | Payload fields | When |
| --- | --- | --- |
| `template.pull.start`, `container.pull.start` | `vm` or `container`, `reference`, `arch` | A download began. |
| `template.pull.progress`, `container.pull.progress` | subject, `reference`, `bytes_done`, `bytes_total`, `percent`, and `chunk`, `chunks` for a template or `layer`, `layers` for an image | The transport reported progress. |
| `template.pull.done`, `container.pull.done` | subject, `reference` | The download completed. |
| `template.pull.cancelled`, `container.pull.cancelled` | subject, `reference` | The `vmlab pull` waiting on it was interrupted. The job stays pending for a retry. |
| `template.pull.error`, `container.pull.error` | subject, `reference`, `error` | The download failed. The job stays pending. |

### Networking and shares

| Event | Payload fields | When |
| --- | --- | --- |
| `smb.started` | `port` | The bundled smbd came up on that host port for the lab's shares. |
| `smb.failed` | `error` | The share plan could not be computed, or smbd failed to start on every candidate port. |
| `forward.installed` | `what`, `bind`, `host_port`, `auto`, `declared_port`, `guest` | A forward is listening on that host address (`bind`, `0.0.0.0` for every interface) and port, the one bound (vmlab's pick when `auto` is true), and leads to `guest` (`ip:port`). `declared_port` is the fixed port declared when a worktree lab's port slot shifted it (PRD §21.7), else `null`. Emitted again each time it is re-installed: at readiness, at the end of `up`, and after a restart changes the lease. |
| `forward.skipped` | `what`, `reason` | The forward plan dropped a declared forward, or installing one failed at runtime. |
| `forward.conflict` | `host_port`, `claimants` | Two declarations claim the same host port. |
| `segment.peer.up` | `segment`, `peer`, `direction` | A cross-host trunk for a global segment came up. Host-scoped: emitted by the supervisor with no `lab`. |
| `segment.peer.down` | `segment`, `peer`, `direction` | A trunk slot cleared, or the segment was torn down. |

### Playbooks

Every `playbook.op.*` event carries `machine`, `playbook` (the path), `play` and
`op_id`, plus the fields below.

| Event | Payload fields | When |
| --- | --- | --- |
| `playbook.op.start` | base, `mode`: `apply` or `check` | A run was admitted. |
| `playbook.op.log` | base, `line` | One human-readable line from the run. Every `playbook.op.*` payload carries `***` in place of a `secret = true` value. |
| `playbook.op.phase` | base, `phase`: `running` or `rebooting`, `attempt`, `max` | The run changed phase, so a reboot shows as one rather than a stall. |
| `playbook.op.step` | base, `cw`: the engine's structured event | One structured line from the guest engine. |
| `playbook.op.done` | base, `exit_code`, `reboots`, `report` | The run finished without an infrastructure error, whatever its exit code. |
| `playbook.op.error` | base, `error` | The agent, the push or the exec failed. |
| `playbook.applied` | `machine`, `playbook`, `play` | An apply exited 0. |
| `playbook.failed` | `machine`, `playbook`, `play`, `mode`, and `exit_code` when the run ran | The run exited non-zero, or failed before it could run. |

### Template builds and pushes

The supervisor emits these for `vmlab template build` and `vmlab template push`.
Every payload carries `template`, `arch` and `kind`, which is `build` or `push`.
See templates.md.

| Event | Payload fields | When |
| --- | --- | --- |
| `template.op.start` | base; `version` for a push | A build or push was admitted. |
| `template.op.console` | base | The build VM's console socket is available. |
| `template.op.step` | base, `event`, `data` | The build's synthetic lab emitted a `playbook.*` event, forwarded with its name and payload inside. |
| `template.op.log` | base, `line` | One non-blank output line from a build or push. |
| `template.op.done` | base, `version` | The build sealed that version, or the push completed. |
| `template.op.cancelled` | base; `version` for a push | The `vmlab template build` or `push` driving it was interrupted, and the supervisor aborted the operation. |
| `template.op.error` | base, `error`; `version` for a push | The build or push failed. |
| `template.built` | `arch`, `name`, `version` | The template landed in the store. Emitted on the build's own synthetic lab, so it appears in that lab's history file rather than in the supervisor stream. |

### Workspace syncer

Every workspace event carries `machine`. See dev-machines.md for what each
condition means and how it is resolved.

| Event | Payload fields | When |
| --- | --- | --- |
| `workspace.identity` | `reason` | At `up`, the dev machine declares no default login, so the tree lands owned by the agent identity. |
| `workspace.unavailable` | `workspace`, `reason` | The declared host workspace directory does not exist. |
| `workspace.degraded` | `reason`, and `path` where one directory is meant | A precondition could not be met: a non-elevated Windows login, a directory that refuses the case-sensitivity flag, or line-ending conversion that could not be turned off. |
| `workspace.stopped` | `reason` | The host file watcher could not start; the syncer gave up. |
| `workspace.deferred` | `reason`, `retry_in_s`; or `reason`, `paths` for a held `.git` lock | A pass, the guest watch or the post-restore re-convergence failed and will be retried; or paths under a `.git` lock were left for the next pass. |
| `workspace.rescan` | `reason` | The guest watch overflowed or its channel dropped, so the guest tree is walked again and both directions block until it completes. |
| `workspace.unwatched` | `directory` | A host subtree could not be registered with the watcher. |
| `workspace.skipped` | `path`, `reason` | A walk skipped a path. |
| `workspace.refused` | `path`, `size`, `cap`, `reason` | A file exceeds the size cap and was refused before transfer. |
| `workspace.case_collision` | `paths`, `reason` | Two paths differ only in case where the guest cannot tell them apart. |
| `workspace.volume` | `path`, `paths`, `bytes`, `reason` | A large burst of changes under one prefix. A warning; it never halts. |
| `workspace.halted` | `reason`, `rules_changed`, `paths` (each `path` and `reason`, capped), `total`, `resolve` | Conflicting changes on both sides halted the whole workspace until `vmlab dev sync resolve`. |
| `workspace.left_standing` | `path`, `reason` | One side dropped a directory the other still has content in. |
| `workspace.symlink_refused` | `path`, `reason` | The guest would not take a symlink. |
| `workspace.failed` | `path`, `reason` | One apply failed, or the halt marker could not be written into the guest. |
| `workspace.synced` | `guest_placed`, `guest_removed`, `host_placed`, `host_removed`, `adopted` | A pass moved something. |
| `workspace.reseed_owed` | `workspace`, `reason` | A restore failed after the ledger was marked rewound, so the re-seed still runs. |
| `workspace.reconverged` | `placed`, `removed`, `adopted`, `reason` | The post-restore re-seed completed. |

## Playbooks

A playbook is a config-weave play applied inside a guest. Where a provision
script is imperative, a playbook is declarative convergence: it describes the
state a machine should be in, `check` reports the drift and `apply` closes it.
vmlab does not interpret the playbook; it pushes config-weave and the playbook
folder into the guest, runs the guest binary, streams its progress, and reboots
the guest when the play asks. The task and module vocabulary inside a
`playbook.wcl` is config-weave's, not vmlab's.

### Declaring one

A `playbook {}` block is declared inside the `vm {}` or `container {}` it
converges, or inside a `template {}` for the build VM. Its label is the playbook
folder, relative to the lab root, and the folder must contain a `playbook.wcl`.
`play` names the play inside it. `var` children are variable overrides scoped to
this machine's run. Block fields are in vm.md.

```wcl
# vmlab.wcl
vm "buildbox" {
  template = "x86_64/linux-modern"
  nic { nat = true }

  provision "scripts/setup.ws" { }
  playbook "playbooks/baseline" {
    play = "baseline"
    var "tz" { value = "UTC" }
  }
}
```

Each `var` becomes one `name = value` line in a variables file vmlab writes for
the run and passes as `--var-file`, in declaration order. config-weave reads
the value as it reads a `--var` flag: as a WCL expression where it can, so `3`
is an integer and `true` a boolean, and as a string otherwise. That is how one
play takes different settings on different machines. `vmlab validate` rejects a
variable name that is not a WCL identifier and a name set twice on one block.

**Secret variables.** Mark a password or token `secret = true`. vmlab then
prints `***` in place of the value in everything it emits for the run — the CLI
stream, the lab log, `playbook.op.*` events, the run report, errors and `vmlab
playbook list` — catching it as written and as config-weave prints it once
evaluated. The run's `variables:` line names variables only, never values. No
variable is ever on the guest command line: the variables file is written for
each run into `<dir>/vars` (`0700`, the file `0600`; SYSTEM and Administrators
only on Windows) and removed when the run ends, timed out or not. config-weave
does not know which variables are secret, so its own `--log-file` and anything
a step writes in the guest can still hold the value.

### Where the binary comes from

config-weave is not bundled with vmlab. It cross-builds exactly two guest
targets, both x86_64: one for Linux and one for Windows. vmlab looks for them in
a directory resolved in this order:

1. `config_weave_bin_dir` in the host configuration file (host-profiles.md).
2. The `VMLAB_CONFIG_WEAVE_DIR` environment variable.
3. `~/.local/share/config-weave/bin`, which is where config-weave's own
   `just install` puts them.

Before anything boots, `vmlab up` checks that the binary each targeted machine
will need exists on the host and fails naming it if not. A guest whose
architecture is not x86_64 cannot run a pushed binary.

The pushed binary and the playbook folders land in one guest directory:
`/var/lib/vmlab/weave` on Linux and `C:\weave` on Windows. The Linux default is
under `/var` because `/var` stays writable on a guest whose root is read-only,
such as an image-based or verity-rooted system, and it persists across reboots.
`guest_dir` on the block moves it for that block, and `playbook_dir` on a guest
OS profile moves it for every machine running that profile. The block wins over
the profile, and the profile over the default. vmlab creates the directory
before pushing; if it cannot, the error names the directory.

Some images ship config-weave themselves. For those, `guest_bin` names the
binary already in the guest, as an absolute path. vmlab then pushes no binary
and needs no host-side one, so the preflight above and the x86_64 limit do not
apply to that block. Before each run it probes `<guest_bin> version`, and fails
naming the path if the binary is missing or does not answer. The playbook
folder is still pushed to the guest directory.

```wcl
playbook "playbooks/baseline" {
  play      = "baseline"
  guest_dir = "/var/lib/weave"           // where the playbook folder goes
  guest_bin = "/usr/bin/config-weave"    // run this one; push none
}
```

`vmlab validate` rejects a `guest_dir` or `guest_bin` that is not absolute for
the machine's guest family: a drive-letter path on Windows, a path starting with
`/` on Linux and macOS, and either where the family is unknown. On Windows a
`guest_bin` must also end in `.exe`. On macOS a playbook must name a `guest_bin`:
config-weave ships no macOS binary for vmlab to push.

### What a run does

A run, whether from `vmlab up` or from `vmlab playbook check` or `apply`, waits
for the machine's agent, then does five things in order.

1. **Make the guest directory.** The block's `guest_dir`, else the profile's
   `playbook_dir`, else `/var/lib/vmlab/weave` on Linux and `C:\weave` on
   Windows. It is created if missing.
2. **Ensure the guest binary.** vmlab remembers the SHA-256 of the binary it
   last pushed to each machine, and the guest path it pushed it to, and probes
   the guest with `config-weave version`. It pushes again when the host binary
   changed, the guest directory moved, or the probe fails, which is what
   happens after a snapshot restore rolls the disk back under a warm cache.
   The host-side files are named `config-weave-linux-x86_64` and
   `config-weave-windows-x86_64.exe`; in the guest they land at
   `<dir>/config-weave` on Linux and `<dir>\config-weave.exe` on Windows. A
   block naming `guest_bin` skips the push and probes that binary instead.
3. **Push the playbook folder, every time.** The folder lands under
   `<dir>/playbooks`. The guest copy is removed and
   re-pushed on each run so deleted source files never linger. The lab-relative
   path is flattened into one guest directory name, `playbooks/baseline`
   becoming `playbooks__baseline`, so two playbooks in one lab cannot collide. A
   fast edit-then-run loop is the point of always pushing.
4. **Run the verb.** The guest command is
   `config-weave check|apply <dir> <play> --json --events-ndjson` plus
   `--var-file <dir>/vars/vars.wcl` when the block has variables. Progress events arrive on stderr as JSON lines and are
   rendered into human lines on the CLI stream and the lab log; the final
   `--json` report on stdout is parsed and attached to the completion event.
5. **Report the verdict.** Infrastructure failures, a push that will not land or
   a guest that stops answering, are errors. config-weave's own verdict comes
   back as its exit code: 0 for converged, 1 for a step error, 2 for a
   validation failure, and 3 for reboot still required.

Both push steps retry with backoff, five attempts over roughly 36 seconds,
because a Windows guest can hold a freshly written file briefly: a lingering
config-weave process or antivirus scanning the new binary shows up as a "file in
use" sharing violation, and the pushes are idempotent. Every retry is announced
so a streamed run shows what it is waiting on.

Each config-weave invocation is bounded by the block's `timeout` (default `1h`),
and the run after a reboot gets the full value again. A run that times out ends
config-weave and nothing else: an installer one of its steps started keeps
going. The run executes in a process tree the guest agent tracks (a subreaper on
Linux, a job object on Windows), and the timeout is recorded against the
machine. The next `check`, `apply` or `up` run on that machine is refused while
anything in that tree is alive, so it cannot start a second installer over the
first; once the tree has drained the record clears and the run goes ahead.
`vmlab playbook list` shows the record. In a container an orphan of an orphan
goes to the container's init and is not counted, and on Windows work handed to
the Windows Installer service is counted only through the `msiexec` waiting on
it. A guest whose agent predates process trees runs as before, with no record
and no refusal.

Only one run per machine is in flight at a time; a second `apply` against a
machine already converging is refused rather than queued. The run emits
`playbook.op.start`, `playbook.op.log`, `playbook.op.step`, `playbook.op.phase`,
`playbook.op.done` or `playbook.op.error` as it goes, and then
`playbook.applied` on a converged apply or `playbook.failed` on any non-zero
exit or error. The last two are the ones an `on {}` handler may bind.

### Reboots

When `apply` exits 3, the play needs a reboot to finish. vmlab reboots the guest
through the agent, waits for the agent to stop answering and then to answer
again, and runs the same apply once more. It does this at most three times; if
the play still reports reboot-required after the third, the run returns exit 3
and says it gave up. The wait for the guest to come back is bounded at ten
minutes and narrated every thirty seconds, because a domain controller's first
post-promotion boot can be quiet for a long time and would otherwise read as a
hang. A `check` never reboots.

A container micro-VM cannot reboot in place: it restarts from a fresh rootfs, so
an apply that asks for a reboot on a container fails with a message saying so
rather than looping.

### Ordering with provisions

Provisions and playbooks are one ordered list per machine. They run in the order
their blocks appear inside that machine, once it is ready. So in the example
above `scripts/setup.ws` runs, then the `baseline` play. Other machines run their
own lists at the same time. A machine with `depends_on = ["buildbox"]` starts as
soon as `buildbox` is ready, without waiting for that list; one with
`depends_on_provision = ["buildbox"]` holds its own first step until `buildbox`'s
list has completed. A template's
playbooks interleave with its provisions the same way during a build, with steps
streamed as build progress.

Both run as the machine's agent identity, SYSTEM on Windows and root on Linux. A
playbook has no user parameter and no rung on the identity ladder
(logins.md), which is a real limit: the agent identity can write into a
profile directory that already exists but cannot create one or set its
ownership, so a play that writes into a user's home half-works on an existing
profile and fails on a fresh domain user. Anything that must land as a
particular login belongs in a provision script using `as_login`.

### On demand

`vmlab playbook apply <machine>` re-pushes and re-runs a declared playbook
without a full `vmlab up`, and `vmlab playbook check <machine>` reports drift
without changing anything. When a machine declares more than one, `--playbook`
and `--play` pick which; the error names the candidates otherwise.
