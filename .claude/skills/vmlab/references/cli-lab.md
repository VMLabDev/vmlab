# vmlab CLI: lab-level verbs

Lab-scoped commands: `up`, `down`, `status`, `validate`, `destroy`, `pull`,
`lab`, `logs`, `eventlog`, `tail`, `dns`, `fastpath`, `playbook`, `script`.
Per-machine verbs are in cli-machine.md; template verbs in cli-template.md.

Exit codes shared across verbs: 0 success, 1 `failed`, 2 usage error,
3 reboot still required, 4 `not_found`, 5 `conflict`, 6 `unsupported`.

## vmlab up

Brings the lab in the current directory to life: validates the lab file, starts
the lab daemon, downloads any registry template or image not yet in the store,
creates each machine's clone, boots the machines in dependency order, and runs
their provision scripts and playbooks. With machine names it does the same for a
subset of the lab.

```sh
vmlab up [VMS]...
```

| Option | Meaning |
| --- | --- |
| `[VMS]...` | Machines to bring up. With none given, every VM and container in the lab. |
| `-h`, `--help` | Print help. |

### What it does

A full validation of the lab file runs before any side effect. A lab that does
not validate is never started, and the issues are printed with the offending
text underlined in the source. A lab that loads also refreshes the managed block
in `~/.ssh/config` (see logins-and-ssh.md), so its machines appear in an editor's
host picker after the first command run in the directory.

It then connects to the supervisor, starting it if none is running, and asks it
for the lab daemon. The lab daemon owns everything that follows and outlives this
process: the workspace syncer, the network fabric and the machines keep running
after `vmlab up` returns (see architecture.md).

Inside the daemon, `up` works through a plan computed before anything is touched:

1. Machines the subset drags in through `depends_on` are added, and the machines
   are grouped into waves. A machine's dependencies must be ready, and the
   provisions scoped to them complete, before the machine's wave starts. Anything
   the plan skips is printed with its reason.
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
6. Between waves the daemon runs every provision script and playbook whose machine
   has started, in declaration order, waiting for each machine's readiness first.
7. Port forwards are installed, a warning is printed for any machine whose agent
   cannot serve an attach (see logins-and-ssh.md), and the workspace syncer starts
   for every dev machine that declares a workspace (see dev-machines.md).

Output from provision scripts streams to the terminal as it happens. On success
the last line is `lab "<name>" is up`. A VM declared with `gui = true` (or a lab
that sets it) gets a detached VNC viewer opened from this terminal, since the
daemon is headless; closing the viewer only disconnects, the VM keeps running.

`up` is idempotent for a machine that is already running: the daemon leaves it
alone. Clones persist across `vmlab down` and are only removed by `vmlab destroy`,
so a second `up` after a `down` boots the same disks and skips the first-boot
script.

Note: a first-boot provision that errors, or that takes longer than 30 minutes,
fails `up` but does not stop the machine, so a console or shell can be opened to
inspect it.

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
that does not come up, or a boot or provision failure all exit 1 (`failed`).
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
3. A hard kill.

A container asks its init to signal the entrypoint and power off after the image's
stop grace, then falls through to the agent and to a kill. `--force` skips the
ladder and kills immediately; the guest gets no chance to flush, so a qcow2 clone
can lose unflushed writes.

A full `down` (no machine names) also stops the bundled SMB server, so it does not
hold its port against the next `up`. A partial `down` keeps shares served for the
machines still running. The lab daemon itself stays up; use `vmlab lab stop` to
stop a lab by name from another directory, or `vmlab destroy` to remove everything.

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
of each segment, and any download in flight. It reads the lab daemon's status
projection and never starts anything.

```sh
vmlab status [OPTIONS]
```

| Option | Meaning |
| --- | --- |
| `-v`, `--verbose` | Add the raw power state, readiness, and each machine's kind-specific detail (template/hardware, image/health/last exit). |
| `-h`, `--help` | Print help. |

### What it prints

With no lab daemon running the verb prints `lab "<name>": not running` and exits 0.
Otherwise it prints up to four sections, each omitted when empty.

#### Machines

One row per machine with the columns `NAME`, `KIND` (`vm` or `container`),
`STATUS`, `IP` and `TEMPLATE/IMAGE`. `STATUS` is a label the daemon derives from
the power state, readiness and health, so every surface words a machine the same
way:

| Label | Meaning |
| --- | --- |
| `starting` | The process is being launched, or a container is up but its entrypoint has not signalled ready. |
| `booting` | A VM is running but its guest agent has not answered yet. |
| `running` | Running and ready. |
| `unhealthy` | A container is ready but its declared healthcheck is failing. |
| `stopping` | The stop ladder is in progress. |
| `stopped` | Powered off. |
| `exited (N)` | A container's entrypoint exited with a non-zero code. |

`IP` is the first NIC with a lease, as reported by the guest agent, or `-` before
the guest is ready. With `--verbose` a second line under each machine carries
`state=`, `ready=`, `cached=` and `attachable=`, plus `diverged=yes` on a machine
whose agent was replaced by `vmlab machine repair-agent` (see cli-machine.md). A VM
then adds `arch`, `cpus`, `memory` and `agent` (the sealed agent version); a
container adds `health`, `exit` and `digest`.

#### Dev machines

Labs with `@dev` machines get a second table: `DEV`, `DEFAULT` (whether it is the
lab's default dev machine), `ATTACH` (whether its agent can serve an attach at all),
`WORKSPACE` (the host directory) and `GUEST WORKSPACE`. A halted workspace syncer
prints its halt sentence under the machine's row, followed by
`vmlab dev sync status <machine>` as the place to read more (see dev-machines.md).

#### Segments

One row per lab segment: `SEGMENT`, `SUBNET`, `GATEWAY`, `NAT/DHCP` as `on/off`
pairs, `DROPPED` (frames the switch shed on this segment; anything other than 0
means the fabric is losing frames under load) and `PEER` (the cross-host trunk
target and whether it is `up` or `down`).

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
nothing else.

One thing does happen on success: the lab is registered in the managed block of
`~/.ssh/config`, because every command that loads a lab does that (see
logins-and-ssh.md). The lab file itself is not written, which is what "no side
effects" refers to. A failure to write the block is a warning, not an error.

Nothing here probes a running machine. The attachability of a dev machine is checked
at `up` and at attach, deliberately not at `validate`, because it depends on a live
agent handshake (see logins-and-ssh.md).

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

1. It withdraws every SSH alias the lab publishes, while the stanzas in the managed
   `~/.ssh/config` block still resolve, so any `ssh` multiplexer holding a connection
   is told to exit (see logins-and-ssh.md).
2. With a lab daemon running, it asks the daemon to destroy the lab. The daemon stops
   every workspace syncer, force-stops every machine, waits up to 30 seconds for each
   to settle, then removes `.vmlab/` and each machine's runtime directory.
3. With no daemon running, it removes `.vmlab/` itself if the directory exists.
4. It asks the supervisor to release the lab, which reaps the lab daemon.

The last line printed is `lab "<name>" destroyed`.

Everything a machine had that was not in its template goes: snapshots live inside the
clones, so they go with them (see snapshots-vision.md), and a machine marked diverged
by `vmlab machine repair-agent` comes back on its template's sealed agent. The
`vmlab dev use` selection recorded in `.vmlab/` is forgotten too, and so is the
workspace sync ledger.

Warning — the workspace survives, the guest tree does not: a dev machine's source
lives in the host workspace directory, which `destroy` never touches. The guest copy
is deleted with the clone. Any guest edit the syncer has not carried to the host is
lost, so run `vmlab dev sync flush` first if the syncer was halted.

Templates in the store are never written to by a lab and are not removed here; use
`vmlab template` for that (see cli-template.md). To destroy a lab by name from any
directory use `vmlab lab destroy`.

### Examples

Tear a lab down and rebuild it from scratch:

```sh
vmlab destroy
vmlab up
```

### Exit status

0 when the lab's state was removed, whether or not a daemon was running. A lab
directory that cannot be found, a machine that cannot be stopped, or a `.vmlab/`
directory that cannot be removed exits 1 (`failed`). A usage error exits 2.

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
act on the daemons behind it (see architecture.md).

```sh
vmlab lab <COMMAND>
```

| Subcommand | Meaning |
| --- | --- |
| `list` | List every tracked lab: name, state, and directory. |
| `info` | Show detailed status (machines and segments) of a running lab. |
| `stop` | Gracefully stop a running lab; clones retained. |
| `restart` | Restart a lab's daemon so it re-reads `vmlab.wcl`. |
| `destroy` | Stop a lab and delete its clones and local state. |
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
`running`, `stopping` or `failed`, the last meaning the daemon exited without being
asked to. With no supervisor or an empty registry it prints `no running labs`. Under
`--json` each entry carries `name`, `root`, `pid` and `state`.

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

Stops every machine in the named lab through the graceful ladder `vmlab down`
describes, or kills them under `--force`, and keeps the clones. Prints
`lab "<name>" is down (clones retained)`. A lab with no reachable daemon prints
`lab "<name>" is not running` and exits 0; unlike `down` this form reaps no orphaned
processes, since without the registry entry it has no directory to release.

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
daemon. It is the way to pick up an edit to the lab file (see lab-file.md), and the way
to recover a daemon whose lab file no longer loads.

The lab must already be stopped. A fresh daemon cannot re-adopt machines the old one was
running, and the old daemon's own shutdown stops them, so a lab with machines still
running is refused with `lab "<name>" still has machines running — stop them first; a
restarted daemon cannot re-adopt them`. A daemon that cannot answer a status request at
all is not a veto, because a lab whose file no longer loads is exactly what this verb
exists to recover.

Run inside the lab's own directory, the current directory is the root the supervisor is
asked to restart from; run elsewhere, the registry's entry is used. The supervisor
refuses a name already registered from a different directory. On success the verb pings
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

The host-wide form of `vmlab destroy`: withdraws the lab's SSH aliases, asks the daemon
to stop every machine and delete the clones, volumes and lab-local state, and releases
the lab at the supervisor so its daemon is reaped. With no reachable daemon but a
registry entry, it removes the lab's `.vmlab/` directory itself. A name the registry does
not know fails with `lab "<name>" is not running`. Prints `lab "<name>" destroyed`.

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

Pick up an edited lab file without re-provisioning:

```sh
vmlab lab stop ad-lab
vmlab lab restart ad-lab
vmlab up
```

Free the host of a lab started from another directory:

```sh
vmlab lab destroy peer-b
```

### Exit status

0 on success, including `list` with nothing running, `stop` on a lab that is not
running, and `info` on a registered lab whose daemon does not answer. An unknown lab
name, a lab with machines still running at `restart`, a daemon that fails the request,
or a supervisor that does not come up exits 1 (`failed`). `restart` exits 5 (`conflict`)
when the supervisor already tracks this name from another directory. A usage error
exits 2.

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
| `[TARGET]` | `[lab/][vm]`. Default: the lab of the current directory. |
| `-f`, `--follow` | Keep following. |
| `-n`, `--lines <LINES>` | Lines of history to show. Default: 100. |
| `-o`, `--output <OUTPUT>` | Output format: `pretty`, human-readable and colorized on a terminal, or `jsonl`, one raw event per line. Default: `pretty`. |
| `-h`, `--help` | Print help. |

With no target the command shows the current lab's `events.jsonl`. A `lab/vm` target
shows that VM's `qemu.log` and `serial.log`. A bare name is a VM when the current
directory's lab declares it and a lab name otherwise, so `vmlab logs ad-lab` works from
anywhere. When no matching log file exists the command refuses with `no logs found`.

Each file is read from its end, so a serial log of tens of megabytes costs only the last
`--lines`. When more than one file matches, each section is introduced with a
`==> path <==` header. Event lines in `pretty` form show the local time, the event name,
and the flattened data; QEMU and serial lines are printed as they are in both formats.
With `--follow` the command polls the first matching file every half second and prints
new lines until Ctrl-C.

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
a run is in progress on that machine, `<check|apply> running since <time>`. A lab with no
blocks prints `no playbook blocks declared in this lab`.

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
with `<kind> of <path> play <play> already running for "<machine>"`.

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
vmlab script <SCRIPT>
```

| Option | Meaning |
| --- | --- |
| `<SCRIPT>` | Script path, relative to the lab root. |
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
