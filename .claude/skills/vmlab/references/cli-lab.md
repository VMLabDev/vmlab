# vmlab CLI: lab-level verbs

Lab-scoped commands: `up`, `down`, `status`, `validate`, `destroy`, `pull`,
`lab`, `worktree`, `logs`, `eventlog`, `tail`, `dns`, `fastpath`, `playbook`,
`script`.
Per-machine verbs are in cli-machine.md; template verbs in cli-template.md.

Exit codes shared across verbs: 0 success, 1 `failed`, 2 usage error,
3 reboot still required, 4 `not_found`, 5 `conflict`, 6 `unsupported`.

Every `vmlab` command takes the global `--instance <ID>`: run this directory's
lab as the instance `<declared>-<ID>`, so one lab file can run from two
directories at once. Without it, `VMLAB_INSTANCE` applies, then the name of the
linked git worktree the lab is in; `--instance ""` (or an empty
`VMLAB_INSTANCE`) runs the lab under its declared name. Every verb here that
finds its lab from the current directory uses that effective name. See
architecture.md ("Lab instances").

## vmlab up

Brings the lab in the current directory to life: validates the lab file, starts
the lab daemon, downloads any registry template or image not yet in the store,
creates each machine's clone, boots the machines in dependency order, and runs
their provision scripts and playbooks. With machine names it does the same for a
subset of the lab.

```sh
vmlab up [--record <VM=PATH>]... [VMS]...
```

| Option | Meaning |
| --- | --- |
| `[VMS]...` | Machines to bring up. With none given, every VM and container in the lab. |
| `--record <VM=PATH>` | Record that VM's console to `PATH` from the first frame of its boot; repeatable, one VM each. Checked before anything boots (one refusal arms none); the recording outlives `up`, which prints `recording <vm> to <path>` after its last line. A VM already running is recorded from now. See cli-machine.md (`vmlab record`). |
| `--fps`, `--size`, `--compress-idle[=SECS]`, `--timeline`, `--max-duration` | Recording options, applied to every `--record`. |
| `-h`, `--help` | Print help. |

### What it does

A full validation of the lab file runs before any side effect. A lab that does
not validate is never started, and the issues are printed with the offending
text underlined in the source.

It then connects to the supervisor, starting it if none is running, and asks it
for the lab daemon. The lab daemon owns everything that follows and outlives this
process: the workspace syncer, the network fabric and the machines keep running
after `vmlab up` returns (see architecture.md).

Inside the daemon, `up` works through a plan computed before anything is touched:

1. Machines the subset names through `depends_on` or `depends_on_provision` are
   added, and `up` prints each one it pulls in. The machines are grouped into
   waves by `depends_on`: a machine's wave starts once every machine it names is
   ready — readiness only, never their provisioning. Anything the plan skips is
   printed with its reason.
2. Deferred downloads run first, with progress streamed to the terminal. This is
   the same code path `vmlab pull` runs on its own.
3. The host's QEMU, firmware and helper binaries are checked for every target
   machine.
4. The bundled SMB server starts when the lab declares shared folders, so shares
   are reachable during provisioning (see shares-media.md).
5. Each wave boots in parallel. A machine carrying a first-boot script runs it
   before it counts as ready. If one machine in a wave fails, the rest of the wave
   is aborted and the verb fails; the machine that failed is left running for
   inspection.
   For a VM `up` waits on anyway (first-boot, a provision or playbook, or a
   dependent), `up` refreshes the agent if it is out of date once it answers,
   after first-boot and before any provision (see "Agent refresh" below).
6. Each machine runs its own provision scripts and playbooks in declaration order
   once it is ready and its share mounts have finished. Machines provision in
   parallel with each other and with later waves still booting. A machine that
   declares `depends_on_provision` holds its first step until every machine it
   names has finished all of its steps. Step output is prefixed with the
   machine's name. The first failed step fails `up` and cancels the steps still
   running; `up` returns once every step has finished. Shared folders are
   mounted once each VM is ready, after its first-boot script, and a mount step
   may retry for five minutes; a VM's provisions and playbooks wait for its mounts
   to finish, and `up` prints its mount report before the first of them runs.
7. Port forwards are installed, and the workspace syncer starts for every dev
   machine that declares a workspace (see dev-machines.md).
8. `up` waits for the share mounts of every VM no provision or playbook waited on,
   ending as soon as every mount task has finished and at most 30 seconds after
   the rest of `up` is done, so a VM with shares that nothing else waits on holds
   `up` until its guest answers and its shares are mounted, or for those 30
   seconds. Each share that will not mount, or is still failing when the wait runs
   out, gets a `WARNING:` line naming the VM, the share and the reason
   (shares-media.md).

Output from provision scripts streams to the terminal as it happens. On success
the last line is `lab "<name>" is up`. A VM declared with `gui = true` (or a lab
that sets it) gets a detached VNC viewer opened from this terminal, since the
daemon is headless; closing the viewer only disconnects, the VM keeps running.

`up` is idempotent for a machine that is already running: the daemon leaves it
alone. Clones persist across `vmlab down` and are only removed by `vmlab destroy`,
so a second `up` after a `down` boots the same disks and skips the first-boot
script.

`up` always runs the lab file as it is on disk now. The lab daemon reads
`vmlab.wcl` once. A full `down` reaps it, but it outlives a partial `down`, a
`vm stop` of every machine and a failed `up`, so before starting anything `up`
compares a digest of what the file declares (comments do not count) with the one
the daemon loaded. Changed, with no machine running and no download in flight:
`up` replaces the daemon and prints `lab "<name>": vmlab.wcl changed — restarting
the lab daemon to load it`. Clones, snapshots, `.vmlab/state.json` and workspace
ledgers persist. Changed while a machine runs (or starts, stops, is suspended): `up`
refuses, names those machines, and changes nothing:

```
lab "ad-lab": vmlab.wcl has changed since the lab daemon loaded it, and dc01 is still running — the daemon cannot load the new file without stopping it. Run `vmlab down`, then this command again, to apply the edit (or undo the edit)
```

A download still in flight refuses the same way and names the artefact; wait for it
to finish, or interrupt the `vmlab pull` that started it.
`pull`, `vm start`/`restart` and `container start`/`restart` follow the same
rule. A VM whose clone was made from a different `template` than the file now
names (recorded in `.vmlab/vms/<vm>/clone-source`, compared as written, so an
unpinned reference survives newer builds, while pinning a different `@<version>`
counts as a different template) is refused before anything downloads or boots,
keeping the clone:

```
vm "web": its disk is a clone of "x86_64/alpine-3.23", but vmlab.wcl now declares "x86_64/debian-13" — a clone cannot move to another template. Run `vmlab vm destroy web` (or `vmlab destroy`) to discard the disk and recreate it from "x86_64/debian-13", or put the template line back
```

`vmlab vm destroy <vm>` (or `vmlab destroy`) recreates it, or put the template line
back. A container's edited `image` line just drops its pin.

A lab daemon also decides, when it starts, whether the lab runs under worktree
isolation and, in a worktree lab, which machines clone from the main checkout's
freeze (worktrees.md). When the main checkout has been frozen, frozen again or
thawed since, or a machine has lost its disk, `up`, `pull`, `vm start` and
`vm restart` replace the daemon the same way, in the main lab and in a worktree
lab, with the same refusals while a machine is running, and print `lab "<name>":
the main checkout's freeze or a machine's disk changed — restarting the lab daemon
to plan its clones again`.

In a linked git worktree whose main checkout is frozen, `up` builds each machine
that has no disk yet as a clone of the freeze rather than of its template, when the
freeze holds it under the same `template` or `image` reference. It boots as if the
freeze point had just been restored, and every lab of the family runs under worktree
isolation. In a frozen family `up` prints one line for each forward it installs
on a port other than its declared one, `"web": container port declared on host
port 18080 is bound on 18081 (worktree port slot, PRD §21.7)` (networking.md,
"Ports in a frozen family").

Note: a first-boot provision that errors, or that takes longer than 30 minutes,
fails `up` but does not stop the machine, so a console or shell can be opened to
inspect it.

### Agent refresh

When a VM's agent first answers, `up` compares its version stamp (`agent=<rev>`)
with the stamp of the agent this vmlab ships. If they differ, in either direction,
it pushes the shipped agent the way `vmlab machine repair-agent` does, marks the VM
**diverged** (`diverged=yes` in `status -v`), emits `machine.agent_updated`, and
prints `agent: updated "<vm>" (<old> → <new>)`. It runs after the first-boot script
(the template's, written for the agent it sealed) and before every provision.
`up` never starts waiting on a VM just to refresh it: a VM with no first-boot, no
provision or playbook and no dependent (and every `vmlab vm start`) is refreshed in
the background when its agent first answers, its line going to the lab daemon's log;
the event and `diverged=yes` still report it. Either way the VM is **not ready
until the refresh has finished** (succeeded or failed): `status` shows it `booting`, and
`wait_ready`, dependents, `vm.ready` and port forwards wait for it — capped at five
minutes, after which the VM is reported ready and the refresh finishes in the
background. A guest that never answers is never waited for. A VM whose agent is current is held only for the comparison; one that
cannot be refreshed is not held. A failed refresh never fails `up`: it prints
`warning: agent: could not update "<vm>" …` with the reason and the old agent stays
in place. Containers, the legacy agent tier, and VMs whose template sealed no agent
are skipped silently. `agent_update = false` on a `vm` (or on the `lab`, for every
VM; the `vm` wins) keeps the template's agent. **Nothing else reaches the agent
while it is being replaced**: from before the push sends the binary until the new
agent has answered a handshake as a new instance, every other use of the VM's
agent (`exec`, `shell`, `cp`, a script) waits, up to four minutes, then runs on the
new agent; a command already running when the push begins gets up to a minute
to finish before the restart. So `vmlab up && vmlab exec …` works even when the
exec lands in the background refresh — before this it failed with `agent channel
closed during exec` or exit 137. On Windows the service restart lands seconds
after the push asks for it, so the push keeps waiting (within its two-minute
budget) while the old agent still answers, and reports `the service did not
restart onto it` only when the budget is spent — a Server 2025 refresh used to be
reported failed with the restart still pending.

### Examples

Bring up the whole lab in the current directory:

```sh
vmlab up
```

Bring up one machine and whatever it depends on:

```sh
vmlab up client01
```

Run the ad-lab example end to end:

```sh
cd examples/ad-lab
vmlab up
vmlab status
```

### Exit status

0 when every target machine started and every provision step completed. A lab
file that fails validation, a lab directory that cannot be found, a supervisor
that does not come up, a boot or provision failure, an edit that cannot be applied
while machines are running, or a VM whose template changed under its clone all exit
1 (`failed`). A share that will not mount is a warning, not a failure: the machines
are up, so `up` still exits 0.
Exit 5 (`conflict`) means the supervisor already tracks a lab with this name from
a different directory; stop that lab or rename this one. A usage error the
argument parser rejects exits 2.

## vmlab down

Stops the machines of the lab in the current directory, gracefully by default,
and keeps their clones so the next `vmlab up` boots the same disks.

```sh
vmlab down [OPTIONS] [VMS]...
```

| Option | Meaning |
| --- | --- |
| `[VMS]...` | Machines to stop. With none given, every machine in the lab. |
| `--force` | Hard kill instead of the graceful ladder. |
| `-h`, `--help` | Print help. |

### What it does

The verb talks only to a lab daemon that is already running; it never starts one.
With no daemon it releases the lab at the supervisor, which reaps any QEMU, swtpm,
virtiofsd or smbd process an earlier daemon left behind, prints
`lab "<name>" is not running (any orphaned processes were reaped)` and exits 0.

Stopping is the mirror of `up`. A subset pulls in its dependents, and the waves
run leaves first, so a domain controller outlives the members that need it to shut
down cleanly. `vmlab down dc01` therefore also stops everything that depends on
`dc01`. Before any machine goes, the workspace syncer for each dev machine in the
plan is stopped, so it does not spend its retry window looking for a guest that has
gone (see dev-machines.md).

Each VM stops through the graceful ladder (see architecture.md):

1. A shutdown request to the guest agent, waiting up to 30 seconds for QEMU to exit.
2. An ACPI power-down through the QEMU control channel, waiting another 30 seconds.
3. A QMP `quit`, which ends QEMU after it flushes the disk caches, waiting another 30 seconds. This is where a guest that halts at "It's now safe to turn off your computer" (Windows 9x, DOS) stops.
4. A hard kill.

A container asks its init to signal the entrypoint and power off after the image's
stop grace, then falls through to the agent and to a kill. `--force` skips the
ladder and kills immediately; the guest gets no chance to flush, so a qcow2 clone
can lose unflushed writes.

A full `down` (no machine names) also stops the bundled SMB server, so it does not
hold its port against the next `up`, then has the supervisor reap the lab daemon.
That releases the lab's name: `lab list` drops it, `status` says `not running`, and
another directory whose lab runs under the same name (a second clone) can `up` it.
A linked git worktree runs its lab as an instance under a name of its own and does
not need this release.
Clones, snapshots and `.vmlab/` stay; the next `up` starts a fresh daemon that reads
`vmlab.wcl` as it is then. A partial `down` keeps the daemon and shares for the
machines still running. `vmlab lab stop` is a full `down` by name from anywhere;
`vmlab destroy` removes everything. A directory whose lab name is registered from
another directory is refused with exit 5 (`conflict`), so it cannot stop that lab.

### Examples

Stop the whole lab, keeping clones:

```sh
vmlab down
```

Kill one hung machine without waiting for the ladder:

```sh
vmlab down --force buildbox
```

### Exit status

0 when every machine in the plan stopped, and 0 when no lab daemon was running.
A machine that fails to stop, or a lab directory that cannot be found, exits 1
(`failed`). A usage error exits 2.

## vmlab status

Reports what every machine in the current lab is doing, its IP address, the state
of each segment, every installed port forward, and any download in flight. It reads the lab daemon's status
projection and never starts anything.

```sh
vmlab status [OPTIONS]
```

| Option | Meaning |
| --- | --- |
| `-v`, `--verbose` | Add the raw power state, readiness, and each machine's kind-specific detail (template/hardware, image/health/last exit). |
| `--json` | Print the lab daemon's status projection as JSON instead of the tables; a lab with no daemon prints `{"lab": "<name>", "running": false}`. |
| `-h`, `--help` | Print help. |

### What it prints

With no lab daemon running the verb prints `lab "<name>": not running` and exits 0
(as after a full `down`). When the lab's name is registered from another directory,
`status` refuses with the same exit-5 `conflict` as `up`, naming that directory.
Otherwise it prints up to five sections, each omitted when empty (the `RECORDING`
rows follow the machine table).

A lab running under worktree isolation (the frozen main lab, or a worktree lab built
on its freeze; worktrees.md) says so on the line after its name, before any table:

```
worktree isolation: worktree "feat-x", generation 1 at "provisioned"; NAT refuses host addresses, forwards bind 127.0.0.1
  global segment "corp" joins as "corp@feat-x"
```

The main lab reads `frozen main lab` in place of the worktree. One indented line
follows for each global segment the lab joins under another name. The line is what
the lab daemon decided when it started, so after a freeze or a thaw it shows the old
decision until the next boot verb replaces the daemon. With `--json` the lab carries
an `isolation` object: `role` (`main` or `worktree`), `worktree`, `generation`,
`snapshot`, `forward_bind` and `globals`, a list of `segment` and `joined` pairs;
absent when the lab is not isolated. The projection also carries `freeze_stale`,
true when the daemon would decide differently if started now: the main checkout has
been frozen, frozen again or thawed since, or a machine has lost its disk.

`<name>` is the name this directory's lab runs under: the declared name, or
`<declared>-<instance>` in a linked git worktree or under `--instance`. The same lab
file running from another worktree is a different lab; `vmlab lab list` shows them
all.

#### Machines

One row per machine with the columns `NAME`, `KIND` (`vm` or `container`),
`STATUS`, `IP` and `TEMPLATE/IMAGE`. `STATUS` is a label the daemon derives from
the power state, readiness and health, so every surface words a machine the same
way:

| Label | Meaning |
| --- | --- |
| `starting` | The process is being launched, or a container is up but its entrypoint has not signalled ready. |
| `booting` | A VM is running but not ready yet: its guest agent has not answered, or a first-boot script or agent refresh is still running. |
| `running` | Running and ready. |
| `unhealthy` | A container is ready but its declared healthcheck is failing. |
| `stopping` | The stop ladder is in progress. |
| `suspended` | A VM's guest put itself to sleep (ACPI S3). QEMU is alive and holds its memory, but nothing in the guest answers, so agent commands are refused with `conflict`. `vmlab vm start` wakes it; `prevent_sleep = true` on the VM keeps it from staying asleep. |
| `stopped` | Powered off. |
| `exited (N)` | A container's entrypoint exited with a non-zero code. |

`IP` is the first NIC with a lease, as reported by the guest agent, or `-` before
the guest is ready. With `--verbose` a second line under each machine carries
`state=`, `ready=` and `cached=`, plus `diverged=yes` on a machine
whose agent was replaced by `vmlab machine repair-agent` (see cli-machine.md) or by
`up`'s agent refresh. A VM
then adds `arch`, `cpus`, `memory` and `agent` (the running agent's handshake
stamp, or the template's sealed one before an agent answers or while stopped; an
agent restarted inside the guest is handshaken again, so this follows it). A VM
with a `gpu {}` adds `host_gpu=` (the host GPU its last start picked), under
virgl/vulkan `keepawake=` (`on` while the supervisor's helper holds that GPU out
of runtime suspend, `off` when the GPU's `keepawake` does not want it, `lost` once
the helper has exited, `failed` when it could not start; host-profiles.md), `qemu=`
when that GPU ran its own `qemu_binary`, and `env=NAME=value,...`: every variable
vmlab set in that QEMU's environment (the `VK_*`/`VMLAB_*` loader filter and the
host GPU's `qemu_env`), by name with values in full — what `qemu.log`'s
`vmlab: env:` line records, taken at the start, so a host config edited since
does not change what a running VM reports. A container adds `health`, `exit` and
`digest`. `--json` prints the projection itself: each machine carries `kind`, its
label and these fields by name (`host_gpu`, `keepawake`, `qemu_binary`, `qemu_env` as an
object), each omitted where empty.

#### Dev machines

Labs with `@dev` machines get a second table: `DEV`, `DEFAULT` (whether it is the
lab's default dev machine), `WORKSPACE` (the host directory) and `GUEST WORKSPACE`. A halted workspace syncer
prints its halt sentence under the machine's row, followed by
`vmlab dev sync status <machine>` as the place to read more (see dev-machines.md).

#### Segments

One row per lab segment: `SEGMENT`, `SUBNET`, `GATEWAY`, `NAT/DHCP` as `on/off`
pairs, `DROPPED` (frames the switch shed on this segment; anything other than 0
means the fabric is losing frames under load) and `PEER` (the cross-host trunk
target and whether it is `up` or `down`).

#### Forwards

One row per installed port forward (segment `forward {}` and container `port {}`
alike): `FORWARD` (the machine), `HOST` (the host port actually listening),
`GUEST` (`ip:port`, the lease it was installed against) and `PROTO`. A port
vmlab picked because the declaration omitted it is marked `(auto)`, as in
`41234 (auto)`; this table is where an automatic port is read from. A fixed
port a worktree lab's port slot shifted (PRD §21.7) is followed by the declared
one, as in `18081 (18080)`. A forward on one address rather than every
interface leads with it, as in `127.0.0.1:8443` (a declared `bind`, or
isolation's `127.0.0.1`). `--json` carries the same as the lab's `forwards`
list, one object per forward with `machine`, `segment`, `host_port`, `guest`,
`guest_port`, `proto` and `auto`, plus `declared_port` on a shifted forward
only and `bind` on a forward not on `0.0.0.0` only. A script reads one forward's port with `vmlab port` (cli-machine.md). An automatic port
stays the same across machine restarts while the lab daemon runs; it may change
after a full `down` and `up`, which restarts the daemon (see networking.md, `forward {}`).

#### Recordings

While any console recording is live, a `RECORDING` section follows the machines:
machine, elapsed seconds and path per recording. `-v` adds `recording=<path>` to
the machine's detail line; `--json` carries a per-machine `recording` object
`{machine, path, started_at, elapsed_secs, options}`. See cli-machine.md
(`vmlab record`).

#### Downloads

While a template or image is downloading, a table of `MACHINE`, `PULLING`
(`template` or `container`), `PERCENT` and `REFERENCE` shows what an `up` that
looks stuck is waiting on. See `vmlab pull`.

### Examples

```sh
vmlab status
```

```sh
lab "ad-lab"

  NAME     KIND      STATUS   IP           TEMPLATE/IMAGE
  dc01     vm        running  10.10.0.10   x86_64/winsrv2022
  client01 vm        booting  -            x86_64/win11
  buildbox vm        stopped  -            x86_64/ubuntu

  SEGMENT SUBNET         GATEWAY    NAT/DHCP DROPPED    PEER
  corp    10.10.0.0/24   10.10.0.1  on/on    0          -

  FORWARD HOST         GUEST           PROTO
  dc01    13389        10.10.0.10:3389 tcp
  dc01    41234 (auto) 10.10.0.10:80   tcp
```

Show the raw state behind each label:

```sh
vmlab status -v
```

### Exit status

0, including when the lab is not running. A lab directory that cannot be found, or
a daemon whose status reply this binary cannot read (a version mismatch between CLI
and daemon), exits 1 (`failed`). A usage error exits 2.

## vmlab validate

Checks the lab file in the current directory against the schema, the semantic rules
of the product contract, the local template store and the guest OS profiles, and
changes nothing. Every side-effecting verb runs the same check first, so this shows
what `up` would refuse before it refuses it.

```sh
vmlab validate
```

| Option | Meaning |
| --- | --- |
| `-h`, `--help` | Print help. |

### What it checks

The verb finds the lab root by walking up from the current directory to a
`vmlab.wcl`, the way `git` finds a repository. It then parses and schema-checks the
file and runs the semantic validation, which resolves each VM's template through the
store the way `up` will, so a version pin resolves the same way and a half-installed
store entry reads as absent. Container hardware is resolved against the host
architecture and the profile set. Every provision script the lab names is compiled by
the wscript host, so a syntax error in a script is reported here rather than at the
moment `up` reaches it. A registry template that has not been pulled yet is not an
error; it is a deferred download.

On success the verb prints one line,
`ok: lab "<name>" — N vm(s), N container(s), N segment(s)`. On failure it prints
every issue as a report with the offending text underlined in the source, and prints
nothing else. Validation checks the declared name; `<name>` is the name the lab runs
under, `<declared>-<instance>` for an instance (as in a linked git worktree).

Nothing here writes a file or probes a running machine. Whether an agent serves a
feature depends on a live handshake, so `validate` never checks it.

### Examples

```sh
vmlab validate
```

```sh
ok: lab "mixed-lab" — 2 vm(s), 1 container(s), 2 segment(s)
```

### Exit status

0 when the lab file is valid. Any issue, a missing lab file, or a profile set that
cannot be loaded exits 1 (`failed`). A usage error exits 2.

## vmlab destroy

Stops the lab in the current directory and deletes everything it materialised: the
clones, the container overlays and named volumes, the lab-local state under
`.vmlab/`, and the dynamic network configuration. The lab file is untouched. The
next `vmlab up` starts from fresh clones and runs first-boot provisioning again.

```sh
vmlab destroy
```

| Option | Meaning |
| --- | --- |
| `-h`, `--help` | Print help. |

### What it does

1. With a lab daemon running, it asks the daemon to destroy the lab. The daemon stops
   every workspace syncer, force-stops every machine, waits up to 30 seconds for each
   to settle, then removes `.vmlab/` and each machine's runtime directory.
2. With no daemon running, it removes `.vmlab/` itself if the directory exists.
3. It asks the supervisor to release the lab, which reaps the lab daemon.

The last line printed is `lab "<name>" destroyed`.

Everything a machine had that was not in its template goes: snapshots live inside the
clones, so they go with them (see snapshots-vision.md), and a machine marked diverged
by `vmlab machine repair-agent` comes back on its template's sealed agent, unless
the next `up` refreshes it again (`agent_update = false` stops that). The
workspace sync ledger recorded in `.vmlab/` is forgotten too.

Warning — the workspace survives, the guest tree does not: a dev machine's source
lives in the host workspace directory, which `destroy` never touches. The guest copy
is deleted with the clone. Any guest edit the syncer has not carried to the host is
lost, so run `vmlab dev sync flush` first if the syncer was halted.

Templates in the store are never written to by a lab and are not removed here; use
`vmlab template` for that (see cli-template.md). To destroy a lab by name from any
directory use `vmlab lab destroy`.

In the main checkout of a worktree lab family, `destroy` refuses while any linked
worktree's disk backs onto the freeze, with or without a daemon running, and changes
nothing; the refusal names each worktree and the generations it uses, and says to run
`vmlab destroy` in each. With no worktree using the freeze, `destroy` deletes
`.vmlab/freeze/` and `worktree.json` with the rest of `.vmlab/`, so the frozen state is
gone for good. In a worktree lab, `destroy` deletes only the worktree's own overlays
and copies, never a frozen file; it is how a worktree releases the freeze, and the next
`up` there clones from the newest generation. It also releases the worktree's port
slot and the ports recorded for it in the main checkout's `worktree-ports.json`, with
or without a daemon running (worktrees.md).

### Examples

Tear a lab down and rebuild it from scratch:

```sh
vmlab destroy
vmlab up
```

### Exit status

0 when the lab's state was removed, whether or not a daemon was running. A lab
directory that cannot be found, a machine that cannot be stopped, or a `.vmlab/`
directory that cannot be removed exits 1 (`failed`). A main lab whose freeze a
worktree lab still uses exits 5 (`conflict`). A usage error exits 2.

## vmlab pull

Downloads every registry template and container image the lab still needs, without
starting any machine. It is the part of `vmlab up` that can take a long time, split
out to run ahead of time or on a metered link.

```sh
vmlab pull [VMS]...
```

| Option | Meaning |
| --- | --- |
| `[VMS]...` | Machines to pull for. With none given, every machine in the lab. |
| `-h`, `--help` | Print help. |

### What it does

Like `up`, the verb validates the lab file first and starts the lab daemon if none is
running. The daemon then runs the same deferred-download step `up` runs, streaming a
progress line per download to the terminal and the same progress events to the event
feed (see automation.md). A template lands in the store under its resolved name and
version; a container image lands in the digest-addressed image cache. When nothing is
pending the verb returns at once. On success it prints `lab "<name>": templates ready`.

Pressing Ctrl-C cancels the downloads rather than abandoning them. The daemon outlives
this process, so an interrupt that simply walked away would leave transfers running
with nobody watching. The cancel only covers the machines this invocation asked for, so
interrupting `vmlab pull web` does not take down a download another terminal started.
The verb then exits with `interrupted — cancelled the download for ...`. A cancelled
download stays pending in the daemon's ledger; the next `up` or `pull` retries it from
scratch.

Only `template = "registry/..."` references and container images are downloaded. A
template built with `vmlab template build` is already in the store, and `pull` has
nothing to do for it.

### Examples

Fetch everything the lab needs before a demo:

```sh
vmlab pull
```

Fetch only the image one container uses:

```sh
vmlab pull web
```

### Exit status

0 when every download completed or nothing was pending. A validation failure, an
unreachable registry, a failed download and an interrupt all exit 1 (`failed`). Exit 5
(`conflict`) means the supervisor tracks a lab with this name from another directory.
A usage error exits 2.

## vmlab lab

Manages running labs host-wide, by name rather than by the current directory. The
supervisor keeps a registry of every lab daemon it started, and these verbs read it and
act on the daemons behind it (see architecture.md). A `<LAB>` argument is the name
the lab runs under, the effective `<declared>-<instance>` for an instance, as
`vmlab lab list` shows it.

```sh
vmlab lab <COMMAND>
```

| Subcommand | Meaning |
| --- | --- |
| `list` | List every tracked lab: name, state, and directory. |
| `info` | Show detailed status (machines and segments) of a running lab. |
| `stop` | Gracefully stop a running lab and release its name; clones retained. |
| `restart` | Restart a lab's daemon so it re-reads `vmlab.wcl`. |
| `destroy` | Stop a lab and delete its clones and local state. |
| `move` | Move a stopped lab's working data to the lab in this directory. |
| `-h`, `--help` | Print help. |

The read-only subcommands never start the supervisor: with none running the registry is
empty and `list` says so. None of them starts a lab daemon either; a lab that is not
running is reported, not brought up.

### vmlab lab list

```sh
vmlab lab list [OPTIONS]
```

| Option | Meaning |
| --- | --- |
| `--json` | Emit a JSON array instead of a table. |
| `-h`, `--help` | Print help. |

Prints one row per registered lab with `NAME`, `STATE` and `DIRECTORY`. The state is
`running`, `stopped`, `stopping` or `failed`. `stopped` means the daemon is up with no
machine running, as after `vm stop` of every machine or a failed `up` (a full `down`
or `lab stop` unlists the lab). `failed` means the daemon exited without
being asked to. With no supervisor or an empty registry it prints `no running labs`. Under
`--json` each entry carries `name`, `root`, `pid` and `state`. Two instances of one
lab file are two rows, each with its own name and directory.

### vmlab lab info

```sh
vmlab lab info [OPTIONS] <LAB>
```

| Option | Meaning |
| --- | --- |
| `<LAB>` | The lab's name. |
| `-v`, `--verbose` | Add the raw power state, readiness, and each machine's kind-specific detail. |
| `-h`, `--help` | Print help. |

The host-wide form of `vmlab status`. With the daemon reachable it prints
`directory: <root>` and then the same report `status` prints, with the same
`--verbose` detail. A lab that is registered but whose daemon does not answer, for
example one in the `failed` state, prints one line from the registry,
`lab "<name>" [<state>] (not reachable) directory <root>`, and exits 0. A name the
registry does not know fails with `lab "<name>" is not running`.

### vmlab lab stop

```sh
vmlab lab stop [OPTIONS] <LAB>
```

| Option | Meaning |
| --- | --- |
| `<LAB>` | The lab's name. |
| `--force` | Hard kill instead of the graceful ladder. |
| `-h`, `--help` | Print help. |

A full `vmlab down` by name, from any directory: stops every machine through the
graceful ladder (or kills them under `--force`), keeps the clones, and has the
supervisor reap the daemon, releasing the lab's name. Prints `lab "<name>" is down
(clones retained)`. The way to give a name back when the registering directory is
gone. A registered lab whose daemon does not answer is released (orphans reaped):
`lab "<name>" is not running (released; any orphaned processes were reaped)`. An
unknown name prints `lab "<name>" is not running`. Both exit 0.

### vmlab lab restart

```sh
vmlab lab restart [OPTIONS] <LAB>
```

| Option | Meaning |
| --- | --- |
| `<LAB>` | The lab's name. |
| `--json` | Emit the raw JSON reply instead of a confirmation. |
| `-h`, `--help` | Print help. |

Replaces the lab's daemon so it re-reads `vmlab.wcl`. This is not `down` followed by
`up`: that stops every machine and re-runs provisioning, whereas this replaces only the
daemon. It picks up an edit to the lab file (see lab-file.md) without starting anything,
and recovers a daemon whose lab file no longer loads. It is not needed before `up`, `pull`
or `vm start`/`restart`, which make the same replacement when the file has changed.

The lab must already be stopped. A fresh daemon cannot re-adopt machines the old one was
running, and the old daemon's own shutdown stops them, so a lab with machines still
running is refused with `lab "<name>" still has machines running — stop them first; a
restarted daemon cannot re-adopt them`. A daemon that cannot answer a status request at
all is not a veto, because a lab whose file no longer loads is exactly what this verb
exists to recover.

Run inside the lab's own directory, the current directory is the root the supervisor is
asked to restart from; run elsewhere, the registry's entry is used. The supervisor
refuses a name already registered from a different directory. Run it from the lab's
directory when the lab is not registered (after a full `down` or `lab stop`); by name
elsewhere it needs the registration. A full `down` already reaps the daemon, so
`lab restart` is never needed after one. On success the verb pings
the new daemon and prints `lab "<name>" daemon restarted at <socket>`; under `--json` it
prints the supervisor's reply, which carries the new `socket` path.

### vmlab lab destroy

```sh
vmlab lab destroy <LAB>
```

| Option | Meaning |
| --- | --- |
| `<LAB>` | The lab's name. |
| `-h`, `--help` | Print help. |

The host-wide form of `vmlab destroy`: asks the daemon to stop every machine and
delete the clones, volumes and lab-local state, and releases the lab at the supervisor
so its daemon is reaped. With no reachable daemon but a registry entry, it removes the
lab's `.vmlab/` directory itself. A name the registry does
not know fails with `lab "<name>" is not running`. Prints `lab "<name>" destroyed`.
Like `vmlab destroy`, it refuses a main checkout's lab whose freeze a linked
worktree still uses (worktrees.md).

### vmlab lab move

```sh
vmlab lab move [OPTIONS] <LAB>
```

| Option | Meaning |
| --- | --- |
| `<LAB>` | The name the lab runs under now, as `vmlab lab list` shows it. The `vmlab.wcl` in this directory must declare the same lab: `<LAB>` is its declared name or an instance of it. |
| `--from <DIR>` | The lab root to move from. Defaults to the directory the lab is registered from; required once the name is released, for example after a full `vmlab down` there. |
| `-h`, `--help` | Print help. |

Hands a stopped lab, with its provisioned machines, to another directory whose lab
file declares the same lab, typically another git worktree of the same repository,
without building its machines again. Run it in the directory that should own the
lab. The two directories may run the lab under different names: the main
worktree's `demo` moved into linked worktree `feat-x` arrives as `demo-feat-x`,
the name that directory runs it under. What must match is the lab the two files
declare. The machines keep their disks, volumes and recorded MAC addresses under
the new name.

It refuses, and changes nothing, when this directory's lab file declares another
lab, when a machine of the lab is running (`vmlab down` there first), or when this
directory's working data already holds machine data: a clone, a container's disk
or a named volume (`vmlab destroy` here first). Working data without any, such as
SMB state left by an earlier `up`, is replaced. When the two lab files declare a
machine differently, or one declares a machine the other does not, it warns naming
each one and continues: the next `up` runs this directory's file and refuses a
clone whose template changed. A frozen lab does not move, because its own disks
and every worktree lab's overlays name the frozen files by absolute path: with
worktree labs on the freeze the refusal names them, otherwise it asks for
`vmlab worktree thaw` first (worktrees.md).

It then releases the name as `vmlab lab stop` does, refusing if any process still
holds the old working data, and moves `.vmlab/` (or the `VMLAB_WORK_DIR` directory
keyed by the old root) to the one keyed by this root. On one filesystem that is a
rename. Across filesystems it copies the tree beside the destination, checks every
file's content against the source, renames the copy into place and only then
deletes the source, so a failed copy leaves the source whole. Clones, snapshots,
firmware variables, TPM state and named volumes move with it. It removes
`smb/smb.conf`, which the next `up` writes for the new root, and rebases each
`@dev` machine's sync ledger onto the new root, so the syncer reads what this
checkout holds differently as host-side changes rather than conflicts.

Folders the lab points machines at do not move: shares, container bind mounts and
`@dev` workspaces belong to the checkout. The verb lists each one that does not
exist under this directory, such as a media folder git ignores. Last, it registers
the lab from this directory under this directory's name and prints
``lab "<name>" now lives in <dir> — run `vmlab validate`, then `vmlab up` ``, with
`as "<new name>"` after the directory when the name changed.

### Examples

See what is running on this host:

```sh
vmlab lab list
```

```sh
NAME      STATE      DIRECTORY
ad-lab    running    /home/wil/labs/ad-lab
mixed-lab failed     /home/wil/labs/mixed-lab
```

Give back the name of a lab registered from a directory you cannot reach, keeping its
clones:

```sh
vmlab lab stop ad-lab
```

Free the host of a lab started from another directory:

```sh
vmlab lab destroy peer-b
```

Take over the lab the main worktree built, from a linked worktree you work in now:

```sh
cd ~/src/app-feature
vmlab lab move app-lab --from ~/src/app-main
vmlab up
```

Stop the instance a linked worktree `feat-x` runs, from any directory:

```sh
vmlab lab stop demo-feat-x
```

### Exit status

0 on success, including `list` with nothing running, `stop` on a lab that is not
running, and `info` on a registered lab whose daemon does not answer. `destroy` exits
5 (`conflict`) when worktree labs use the lab's freeze. An unknown lab
name, a lab with machines still running at `restart`, a daemon that fails the request,
or a supervisor that does not come up exits 1 (`failed`). `restart` exits 5 (`conflict`)
when the supervisor already tracks this name from another directory. `move` exits 5
when a machine or process of the lab is running, when this directory already holds
machine data, when the lab is frozen, or when the lab is registered from a directory
other than `--from`; it exits 2 when a lab file runs under another name, and 1 when
the lab is not registered and no `--from` is given. A usage error exits 2.

## vmlab worktree

Freezes the main checkout's lab for worktree labs to clone their machines from,
thaws it, and reports where the family stands (worktrees.md). `freeze` and `thaw`
run in the main checkout and go to its lab daemon, as the snapshot verbs do.
`status` reads the files alone, so it answers in the main checkout or any linked
worktree with no daemon running.

```sh
vmlab worktree <COMMAND>
```

| Subcommand | Meaning |
| --- | --- |
| `freeze` | Freeze the stopped main lab at an offline snapshot every machine has, for linked git worktrees to clone their machines from. |
| `thaw` | Fold the freeze back into the stopped main lab's disks and unlock its earlier snapshots. |
| `status` | Each generation of the main lab's freeze, its snapshot, and the worktrees whose disks back onto it. |
| `-h`, `--help` | Print help. |

A lab daemon decides worktree isolation when it starts, and `freeze` and `thaw` run
in a daemon that started before them. The next `up`, `pull`, `vm start` or
`vm restart` in the main checkout replaces that daemon first, as after an edit to
`vmlab.wcl`.

### vmlab worktree freeze

```sh
vmlab worktree freeze [OPTIONS] <SNAPSHOT>
```

| Option | Meaning |
| --- | --- |
| `<SNAPSHOT>` | The offline snapshot to freeze at. |
| `--json` | Emit the raw JSON reply instead of a report. |
| `-h`, `--help` | Print help. |

Freezes every machine of the lab at `SNAPSHOT` as the next **generation**, `1` for
the first freeze. Each disk is rewound to the snapshot, moved into
`.vmlab/freeze/<n>/<machine>/` and made read-only, and the machine's disk becomes an
overlay on it holding the state the lab had, so the main lab carries on unchanged.
The overlay is written by copying the disk (`cp --reflink=auto`, free on a
filesystem with reflinks) and rebasing an empty overlay onto the frozen file with a
safe `qemu-img rebase`, which writes only the clusters that differ from the
snapshot; without reflinks each copy is a full one, and every disk's copy stays
until the whole freeze has committed. The machine's `OVMF_VARS.fd`, `tpm-state/`,
`clone-source`, `firstboot.done` and state record, and the lab's named volumes, are
copied into the generation. `.vmlab/worktree.json` is written last; its presence is
what makes the lab frozen. A failure at any step undoes every step before it, and
the error says so.

`SNAPSHOT` becomes the **freeze point**, and every other snapshot of the lab is
**locked** in this generation (cli-machine.md, `vmlab snapshot`). Freezing again
freezes the main lab's current overlays as generation `n+1`, backed by generation
`n`, and locks the previous freeze point with the rest. On success it prints:

```
froze generation 1 at "provisioned": dc01, web
snapshots taken before it are locked until `vmlab worktree thaw`; linked worktrees now clone these machines on `vmlab up`
```

then a `WARNING:` line for each leftover file it could not clean up. It refuses, and
changes nothing, when:

- the lab is in a linked git worktree; only the main checkout's lab freezes,
- the lab declares no machines,
- a machine is running (`vmlab down` first),
- a machine has no snapshot `SNAPSHOT`, or has never been built,
- the snapshot was taken online,
- the snapshot is itself locked in an earlier generation (take a new offline snapshot),
- a disk lacks the internal snapshot the lab's record says it holds,
- a disk still holds the temporary snapshot an interrupted freeze or thaw left behind,
- a disk names its backing file by a relative path, which the move would break.

### vmlab worktree thaw

```sh
vmlab worktree thaw [OPTIONS]
```

| Option | Meaning |
| --- | --- |
| `--drop-online-snapshots` | Give up the online snapshots taken since a freeze, whose machine state cannot be carried into the thawed disks, rather than refuse. |
| `--json` | Emit the raw JSON reply instead of a report. |
| `-h`, `--help` | Print help. |

Collapses each disk's chain back into its oldest frozen file, which becomes the
machine's disk again. Every layer above it is committed down with `qemu-img commit`,
and the snapshots each layer holds are carried across one by one, so offline
snapshots taken after a freeze survive. The main lab keeps its current state.
`.vmlab/freeze/` and `worktree.json` are removed and every snapshot is unlocked. On
success it prints `thawed: the freeze is gone and every snapshot is unlocked`; with
`--drop-online-snapshots` it adds `dropped snapshot "<name>" of <machine>` for each
snapshot given up, then any `WARNING:` lines. It refuses when:

- the lab is in a linked git worktree; only the main checkout's lab thaws,
- the lab is not frozen,
- any worktree lab's disk backs onto any generation; the refusal names each
  worktree and its generations, and `vmlab destroy` in each releases it,
- a machine is running (`vmlab down` first),
- a machine holds online snapshots stored above its oldest generation, unless
  `--drop-online-snapshots` is given; the refusal names them,
- a disk's chain is not the shape a freeze leaves: a temporary snapshot left by an
  interrupted freeze, a snapshot name stored twice in one chain, or a generation
  that does not hold its own freeze point.

### vmlab worktree status

```sh
vmlab worktree status [OPTIONS]
```

| Option | Meaning |
| --- | --- |
| `--json` | Emit the raw JSON instead of a table. |
| `-h`, `--help` | Print help. |

Prints the main checkout, then `this worktree:` when run in a linked worktree, then
one row per generation: `GENERATION` (the newest marked `(newest)`), `SNAPSHOT`,
`MACHINES` and `WORKTREES`, the linked worktree labs whose disks back onto it, or
`-`. It reads every worktree disk's backing chain directly, so it counts worktree
labs whose daemons are not running. An unfrozen lab prints `not frozen` with the
command that freezes it. Generations on disk with no `worktree.json` add a warning
that worktree labs do not clone from them.

```
main checkout: /home/wil/src/app
GENERATION  SNAPSHOT     MACHINES   WORKTREES
1           provisioned  dc01, web  /home/wil/src/feat-x
2 (newest)  upgraded     dc01, web  -
```

### Exit status

0 on success, and 0 for `status` on an unfrozen lab. Every refusal listed above
exits 5 (`conflict`). A step that failed and was rolled back, an invalid snapshot
name, or a daemon that cannot be reached exits 1 (`failed`). A usage error exits 2.

## vmlab logs

Dumps or follows the JSON-line logs vmlab writes under its state directory: the lab's
event log, or one VM's QEMU and serial logs. It reads the files directly, so it works
with no daemon running and after a lab has been stopped. The events it shows are
described in automation.md.

```sh
vmlab logs [OPTIONS] [TARGET]
```

| Option | Meaning |
| --- | --- |
| `[TARGET]` | `[lab/][machine]`. Default: the lab of the current directory. |
| `-f`, `--follow` | Keep following. |
| `-n`, `--lines <LINES>` | Lines of history to show. Default: 100. |
| `-o`, `--output <OUTPUT>` | Output format: `pretty`, human-readable and colorized on a terminal, or `jsonl`, one raw event per line. Default: `pretty`. |
| `-h`, `--help` | Print help. |

With no target the command shows the current lab's `events.jsonl`. A `lab/vm` target
shows that VM's `qemu.log` and `serial.log`, and a `lab/container` target that
container's `console.log`. A bare name is a machine when the current directory's lab
declares it and a lab name otherwise, so `vmlab logs ad-lab` works from
anywhere. When no matching log file exists the command refuses with `no logs found`.

Each file is read from its end, so a serial log of tens of megabytes costs only the last
`--lines`. When more than one file matches, each section is introduced with a
`==> path <==` header. Event lines in `pretty` form show the local time, the event name,
and the flattened data; QEMU, serial and console lines are printed as they are in both
formats. With `--follow` the command polls the first matching file every half second and
prints new lines until Ctrl-C.

```sh
vmlab logs -f
vmlab logs -n 500 -o jsonl ad-lab | jq 'select(.event == "vm.state")'
vmlab logs ad-lab/dc01
```

### Exit status

0 on success. The command sends no daemon request, so every failure exits 1: no lab in
the current directory and no target, a lab file that does not load, no log file for the
target, or a file that cannot be read while following.

## vmlab eventlog

Follows the Windows event log of a guest over the vmlab agent channel. It is the
event-log counterpart of `vmlab tail`: a stream that keeps printing as new events are
logged, with no guest network or shell involved.

```sh
vmlab eventlog [OPTIONS] <VM>
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The machine, as `[lab/]name`. |
| `--filter <FILTER>` | XPath filter. Default: everything on the System channel. |
| `-h`, `--help` | Print help. |

The command asks the lab daemon to open an event-log session on the machine's agent and
prints each chunk as it arrives, flushing after every chunk. It runs until Ctrl-C, the
machine stops, or the agent reports a session error. The filter is the same XPath the
Windows Event Viewer accepts in its XML filter, and it selects both the channel and the
events within it.

The daemon checks the agent's negotiated features before opening the session. A guest
whose agent does not advertise the event log, which is every Linux guest and every
container, is refused as `unsupported` with the message that the event log is a
Windows-only feature.

```sh
vmlab eventlog dc01
vmlab eventlog dc01 --filter "*[System[(Level=1 or Level=2)]]"
```

### Exit status

0 when the stream ends. `not_found` (4) means the lab declares no machine by that name.
`unsupported` (6) means the guest's agent has no event log. `failed` (1) covers a machine
that is not running, an agent that does not answer, and a session error such as a filter
Windows rejects.

## vmlab tail

Follows a file inside a guest over the vmlab agent channel, with `tail -F` semantics: it
keeps reading as the file grows and follows it across truncation and replacement. No guest
network and no guest shell are needed.

```sh
vmlab tail <VM> <PATH>
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The machine, as `[lab/]name`. |
| `<PATH>` | Guest file path. |
| `-h`, `--help` | Print help. |

The command asks the lab daemon to open a tail session on the machine's agent and prints
each chunk as it arrives, flushing after every chunk so a pipe sees lines promptly. It runs
until Ctrl-C, the machine stops, or the agent reports an error on the session. The daemon
checks the machine's state every half second and ends the stream when it is no longer
running.

The path is read by the agent's own identity, SYSTEM or root, so files a user cannot read
are still followed. This is the verb for a guest log that has no host-side copy; the lab's
own event log and a VM's serial and QEMU logs are read host-side by `vmlab logs`.

```sh
vmlab tail dc01 C:/Windows/debug/netsetup.log
vmlab tail nix01 /var/log/messages
```

### Exit status

0 when the stream ends. `not_found` (4) means the lab declares no machine by that name.
`failed` (1) covers a machine that is not running, an agent that does not answer, and a
session error such as a path the agent cannot open.

## vmlab dns

Prints the DNS zones the current lab's segments serve: every exact record, wildcard and
sinkhole, in the order the resolver consults them (see networking.md). It is the thing to
read when a guest cannot resolve a peer.

```sh
vmlab dns [OPTIONS]
```

| Option | Meaning |
| --- | --- |
| `--json` | Emit the raw JSON instead of a table. |
| `-h`, `--help` | Print help. |

### What it prints

The verb starts the lab daemon if none is running and asks it for the DNS table. Segments
with no local zone, global segments and segments with `dns { enabled = false }`, serve
nothing and are not listed; with no serving segment at all the verb prints
`no segment in this lab serves DNS`.

Each serving segment gets a heading `segment "<name>" — zone <suffix>` and a table of
`NAME`, `IP` and `KIND`. Exact records come first, sorted by name, with kind `static` for a
record the lab file declares and `dynamic` for one a DHCP lease registered; then wildcards
with kind `wildcard`; then sinkholes with `-` for the address and kind `sinkhole/<mode>`,
since a sinkhole answers with nothing and `NXDOMAIN` and `0.0.0.0` fail differently in a
guest. A zone with no rules prints `(no records)`.

With `--json` the daemon's reply is printed verbatim as pretty JSON, an object with a
`segments` array whose entries carry `segment` and a `zone` with `suffix`, `records`,
`wildcards` and `sinkholes`.

### Examples

```sh
vmlab dns
```

```sh
segment "corp" — zone vmlab.internal
  NAME                              IP          KIND
  client01.ad-lab.vmlab.internal    10.10.0.50  dynamic
  dc01.ad-lab.vmlab.internal        10.10.0.10  static
  *.corp.example                    10.10.0.10  wildcard
  *.telemetry.example.com           -           sinkhole/nxdomain
```

Feed the table to a script:

```sh
vmlab dns --json | jq '.segments[].zone.records[]'
```

### Exit status

0 when the table was printed. A lab directory that cannot be found, or a daemon that could
not be started or answered with a failure, exits 1 (`failed`). Exit 5 (`conflict`) means the
supervisor tracks a lab with this name from another directory. A usage error exits 2.

## vmlab fastpath

Shows which network fast-path tier the supervisor selected for switch traffic, and why the
tiers it skipped were unavailable. The tiers are the substitutable backends of the userspace
fabric (see networking.md).

```sh
vmlab fastpath
```

| Option | Meaning |
| --- | --- |
| `-h`, `--help` | Print help. |

The command starts the supervisor if it is not running, like every other verb, because the
answer is the probe result of the daemon that will carry the traffic. It prints one line
naming the tier and the mode it was selected under, then one line per skipped tier with the
reason.

The tier is one of `afxdp`, tap devices with in-kernel XDP forwarding; `sockmap`, kernel
socket splicing on the stream-socket ports; or `userspace`, the plain switch that is always
available. The mode is the `fastpath` key in the host configuration (see host-profiles.md),
overridden by the `VMLAB_FASTPATH` environment variable: `auto` probes `afxdp` and otherwise
falls back to `userspace`, `off` never uses a kernel path, and `sockmap` or `afxdp` probe
only that tier. `auto` never picks `sockmap`, because it measures slower than the userspace
fabric, and the reasons say so. A forced tier whose probe fails degrades to `userspace`
rather than stopping the daemon. A vmlab built without the `ebpf` feature reports both kernel
tiers unavailable for that reason.

```sh
$ vmlab fastpath
network fast path: userspace (mode auto)
  afxdp unavailable: vmlab was built without the `ebpf` feature
  sockmap unavailable: not used in auto mode: af_unix kernel splicing measures slower than the userspace fabric (psock backlog workqueue); force with `fastpath = "sockmap"` to evaluate it
```

### Exit status

0 on success. The command discards the protocol error code and exits 1 for any failure,
including a supervisor that does not come up.

## vmlab playbook

Runs the config-weave playbooks a lab declares with `playbook {}` blocks against its
machines, on demand (see automation.md). `up` applies them once in declaration order; these
verbs are the edit-then-check loop after that.

```sh
vmlab playbook <COMMAND>
```

| Subcommand | Meaning |
| --- | --- |
| `list` | List the lab's playbook blocks and any in-flight runs. |
| `check` | Report drift without changing the guest (re-pushes the playbook first). |
| `apply` | Push the playbook and converge the guest (auto-reboots on demand). |
| `-h`, `--help` | Print help. |

`list` works on the lab in the current directory. `check` and `apply` take a machine
reference of the form `[lab/]machine`; a bare name is resolved against the current
directory's lab, and the lab daemon is started if none is running.

### vmlab playbook list

```sh
vmlab playbook list
```

| Option | Meaning |
| --- | --- |
| `-h`, `--help` | Print help. |

Prints one line per `playbook {}` block, `<machine> → <path> play <play>`, followed by an
indented `var <name>=<value>` line for each variable override the block declares, and, when
a run is in progress on that machine, `<check|apply> running since <time>`. A block whose last
run hit its `timeout` adds a `timed out at <time>` line saying whether the processes that run
started are still running, have finished, or cannot be asked about because the guest is not
answering. A lab with no blocks prints `no playbook blocks declared in this lab`.

### vmlab playbook check

```sh
vmlab playbook check [OPTIONS] <MACHINE>
```

| Option | Meaning |
| --- | --- |
| `<MACHINE>` | Machine (`[lab/]name`). |
| `--playbook <PLAYBOOK>` | Playbook folder path, when several target this machine. |
| `--play <PLAY>` | Play name, when several target this machine. |
| `-h`, `--help` | Print help. |

Pushes the playbook folder into the guest again, so an edit on the host is what gets checked,
and runs config-weave in check mode: it reports which steps have drifted and changes nothing.
Config-weave's own output streams to the terminal as it runs. When a final report comes back
the verb prints a one-line summary, `check: N ok · N changed · ...`, counting steps by status.

Which block runs is resolved from the machine's `playbook {}` blocks. With exactly one there
is nothing to choose. With several, `--playbook` and `--play` narrow the choice, and a machine
that still matches more than one block, or none, is refused with the candidates named. Only
one run may be in flight per machine; a second `check` or `apply` while one runs is refused
with `<kind> of <path> play <play> already running for "<machine>"`. A run is also refused
while processes started by a run on that machine that timed out are still alive in the guest
(see automation.md).

### vmlab playbook apply

```sh
vmlab playbook apply [OPTIONS] <MACHINE>
```

| Option | Meaning |
| --- | --- |
| `<MACHINE>` | Machine (`[lab/]name`). |
| `--playbook <PLAYBOOK>` | Playbook folder path, when several target this machine. |
| `--play <PLAY>` | Play name, when several target this machine. |
| `-h`, `--help` | Print help. |

Pushes the playbook and runs config-weave in apply mode, converging the guest. When a step
asks for a reboot the daemon reboots the guest, waits for it to come back, and resumes, up to
three times; a guest still asking for a reboot after that fails the run with exit 3. The
summary line adds `rebooted N time(s)` when any reboot happened. Selection and the one-run
rule are the same as for `check`. This is the same run `up` performs for each block, so a
failing `apply` is also what fails an `up`.

### Examples

See what the lab would configure:

```sh
vmlab playbook list
```

```sh
dc01 → playbooks/domain play forest
  var domain=corp.example
client01 → playbooks/workstation play join
```

Edit a playbook on the host, then see the drift before applying:

```sh
vmlab playbook check client01
vmlab playbook apply client01
```

Pick one of several plays that target a machine:

```sh
vmlab playbook apply --playbook playbooks/workstation --play harden client01
```

### Exit status

Exit status for `check` and `apply` mirrors config-weave's when the run completes: 0 when
every step is ok or converged, 1 when a step errored, 2 when the playbook failed validation,
and 3 when a reboot was still required after the bounded retries. Before a run starts, exit 4
(`not_found`) means the machine is not declared or no single playbook block matched the
selection, and exit 5 (`conflict`) means a run is already in flight on that machine. A machine
that is not running, an agent that does not answer, or a push that fails exits 1 (`failed`).
`list` exits 0 whether or not the lab declares any block, and 1 when the lab cannot be
reached. A usage error exits 2.

## vmlab script

Runs an ad-hoc wscript file against the lab of the current directory (see
wscript-language.md). The script runs inside the lab daemon, with the same `lab` binding a
`provision {}` block or an event handler gets, and its output streams back to the terminal as
it is produced.

```sh
vmlab script [--record <VM=PATH>]... <SCRIPT>
```

| Option | Meaning |
| --- | --- |
| `<SCRIPT>` | Script path, relative to the lab root. |
| `--record <VM=PATH>` | Record that VM's console to `PATH` for exactly as long as the run lasts; repeatable, one VM each. Started before the script, finalised when it returns, success or failure; the verb prints `recorded <vm> to <path>`. See cli-machine.md (`vmlab record`). |
| `--fps`, `--size`, `--compress-idle[=SECS]`, `--timeline`, `--max-duration` | Recording options, applied to every `--record`. |
| `-h`, `--help` | Print help. |

The path is resolved against the lab root, the directory holding `vmlab.wcl`, not against the
shell's directory, so the same command works from any subdirectory of the lab. The command
checks the file exists before it asks anything and refuses with
`script <path> not found under <root>` when it does not. It then starts the supervisor and the
lab daemon if they are not running, sends the `run` request, and prints each chunk of output
as it arrives. Machines the script touches must already be up; the script itself can start
them through the Lab API (see wscript-lab-api.md).

`script` is reachable only from the CLI: the file comes from the caller's disk and its output
belongs to the terminal that ran it.

```sh
cd examples/ad-lab
vmlab up
vmlab script scripts/collect-dumps.ws
```

### Exit status

0 when the script completes. A script that raises, or a call inside it that fails, ends the
run with `failed` (1). A missing file or a daemon that does not start exits 1 before any
request is sent.
