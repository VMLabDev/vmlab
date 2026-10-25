# vmlab

A single-host VM lab orchestrator: labs and virtual networks declared in WCL,
reusable disk templates built locally or distributed over OCI registries, and
guest automation written in wscript.

This is the canonical glossary. The manual's glossary
(`docs/manual/pages/appendices/glossary.wcl`) restates the user-facing subset;
when the two disagree, this file wins.

## Language

### Labs and machines

**Lab**:
A set of machines plus the virtual networks connecting them, declared in a
`lab {}` block in `vmlab.wcl`. The name it runs under, its declared name or a
lab instance's effective name, identifies it uniquely among the labs
registered on one host.
_Avoid_: environment, project, stack

**Lab instance**:
A lab file run under the effective name `<declared>-<instance>` rather than its
declared name, so one lab file can run from two directories at once. The
instance comes from `--instance`, `VMLAB_INSTANCE` or the linked git worktree
the lab is in (ADR-0011's amendment).
_Avoid_: copy, clone (a clone is a VM's disk)

**Machine**:
Anything a lab boots, attaches to a segment, and drives through the agent — a
VM or a container. The unit that `depends_on`, `up` waves and `machine.*`
operations address.
_Avoid_: node, instance (a lab instance is a lab), workload

**VM**:
A machine booted from a template's linked clone, with its own firmware, disks
and hardware surface.
_Avoid_: virtual machine (spelled out), box

**Container**:
A machine declared from an OCI image rather than a template. It runs inside a
micro-VM, so it is a lab machine in every respect — same segments, DNS,
snapshots and agent channel as a VM.
_Avoid_: pod, service, workload

**Micro-VM**:
The tiny VM — pinned Alpine kernel plus vmlab's purpose-built init — that each
container runs inside.

**Guest**:
The operating system running inside a machine, as distinct from the machine
itself.

**Segment**:
A virtual layer-2 switch. The lab daemon supplies DHCP, DNS, NAT, routing and
L3 filtering for it in userspace.
_Avoid_: network, VLAN, bridge, subnet

**Scratch VM**:
A VM booted from a blank disk (`template = "scratch"`) with no template,
requiring explicit `arch`, `profile` and `disk`.

**Dev machine**:
A lab machine designated as a development environment with a `@dev` decorator
on its block: a machine with a synced **workspace**. VM or container, Windows
or Linux — one contract for every machine kind. The decorator states something *about* the machine rather than
configuring something *inside* it: nothing it carries is a setting the guest
sees. A lab may have any number, or none.
_Avoid_: devbox, workspace (a workspace is the source tree on one), devcontainer

**Default dev machine**:
The dev machine carrying `@dev(default = true)`, or the only one carrying
`@dev`. A property of the **lab file**, so it is the same for everyone who
opens it — not per-developer. It is the last rung of the fixed ladder a `vmlab
dev sync` verb picks its machine by — argument, `VMLAB_DEV_MACHINE`, default
dev machine, else an error listing the candidates. A rung naming a machine
this lab does not offer is an error at that rung, never a fall through to the
next. See PRD §19.7.

**Workspace**:
A dev machine's source tree: a guest-local working copy on the machine's own
disk, of a host directory that is canonical. Declared with `@dev(workspace)`,
kept in step by the **workspace syncer**, and pointedly not a `share {}` —
a share is virtiofs/SMB passthrough where a workspace is a synced local copy,
which is why the argument is never spelled bare `guest`. See ADR-0014.
_Avoid_: source share, bind mount, project directory

**Guest-owned**:
A path the ignore rules exclude from the workspace. Not *skipped*: the guest is
expected to hold its own diverging content there — `node_modules` with
guest-native binaries is the proving case — so neither sync direction ever
touches it, and it does not survive a rebuild, correctly, because it is
reconstructible.
_Avoid_: ignored (that describes the rule, not the path), excluded

**Ignore floor**:
vmlab's own layer of the workspace ignore rules, under the repo's `.gitignore`
and the developer's `.vmlabignore` and outranking both. It holds the syncer's
own scratch names — an apply's temp file, the halt marker — so no repo rule and
no negation can turn one of them into a sync object.
_Avoid_: default ignores, built-in excludes

**Size guard**:
The workspace syncer's per-file cap, refused **before** transfer and naming the
file, the rule and both ways out (an ignore rule, or a larger cap). Distinct
from a **volume warning**: the guard refuses because a multi-gigabyte file
nobody wrote a rule for is unwanted work, where a build burst is wanted work
that happens to be large.
_Avoid_: quota, limit (alone), throttle

**Reconciliation**:
What one **sync pass** decided, as a value: the actions to carry out, the
paths adopted as agreed, the agreements to drop, the conflicts, and the files
the **size guard** refused. Computed from host state, guest state and the
**sync ledger**, with no I/O in it — so the rules that matter most are
arithmetic rather than a property of a running lab.
_Avoid_: diff, delta, changeset

**Sync pass**:
One reconciliation of a workspace end to end — walk the host, learn what the
guest holds, reconcile against the **sync ledger**, apply, record. What the
guest is asked is the difference between the steady state and the exception
path: the pass **probes named paths** (the host's, the ledger's, and what the
**drain** reported), except on a **watch discontinuity**, where it takes the
**stat-walk**. The **seed** is simply the first one; there is no separate
seeding mechanism.
_Avoid_: sync cycle, run, tick

### Capabilities

**Capability**:
Something one machine can do that another might not — a display, a clipboard,
a Windows event log. Probed and reported, never inferred from whether the
machine is a VM or a container.

**Agent repair**:
Pushing the host's shipped agent binary into a running machine over the
agent's own channel, replacing what its artefact baked — the way to iterate on
the agent without rebuilding a template. The verb is the on-demand form;
**agent refresh** is the same push run by `vmlab up` and `vm start`, by
default, when a VM's agent stamp differs from the shipped asset's (opt out
with `agent_update = false`). Either makes the machine a **diverged machine**.
Meaningless for a machine whose agent came with the host rather than with what
it boots — a container micro-VM's — which is reported rather than implied, and
impossible on the legacy tier, which `up` skips silently. While it runs it
holds the machine's **agent channel**: every other caller waits until the new
agent has answered a handshake under its own instance, so nothing rides the
connection the restart closes.
_Avoid_: upgrade, hot-patch, self-update

**Display**:
A machine's framebuffer, together with the keyboard, pointer, OCR and
image-matching operations that read and drive it. Probed and reported like any
other capability — not inferred from machine kind. No container reports one
today; one running a display server could, so absence is reported as "this
machine has no display", never as "containers cannot have displays".
_Avoid_: console, screen, VNC, framebuffer (alone)

**Shared app**:
One guest program whose windows appear on the host's own desktop, beside the
host's windows, started by `vmlab app run` and ended when the program (or, for
RDP, the client) exits. vmlab writes no window protocol: the guest family's
own one — waypipe, X11, or RDP RemoteApp — rides the agent channel, its host
end runs in the CLI because only a process in the user's desktop session can
reach the compositor, and the lab daemon stays headless. Runs as the declared
login, like `exec`. A CLI act on a running machine, never a declaration and
never a wscript call. Unrelated to the **display**, which it does not use.
_Avoid_: remote app, seamless window, app forwarding, share (that is a folder)

**Console recording**:
A video of one VM's **display**, MP4 or WebM, written by the lab daemon from
the VM's VNC socket while it runs; started by `vmlab record start`, by
`--record` on `script`/`up`/`vm start`/`vm restart`, or by a script's
`Machine.record`, and finalised to a playable file by `record stop`, its
maximum duration, or whatever ends the machine or the run. Draws the pointer
only where vmlab itself sent it, because VNC carries no pointer position. Not
a screenshot (one frame) and not a screen capture of the host.
_Avoid_: screen recording, screencast, capture (that is a snapshot verb), video (alone)

**USB passthrough**:
Handing a USB device plugged into the host to a running VM, and giving it
back, with no restart. Needs the VM to have booted with `usb = true` — an
opt-in hot-plug xHCI controller, because adding it changes the VM's hardware.
A CLI and wscript act on a running machine, never a declaration; a container
refuses it. Unrelated to **shared apps** and to `-usb`'s tablet.
_Avoid_: USB redirection (that is SPICE's client-side feature), USB sharing

**Attachment**:
One host USB device held by one running VM — the device as listed when it was
attached, pinned to that plug-in by bus, port and device number. Runtime only:
tracked by the lab daemon in memory, released by a detach, a VM stop, an
online restore, or the device leaving the host. The supervisor tracks none; its
listing asks every lab who holds what.
_Avoid_: claim, binding, mount

**Selector** (USB):
How a person names one host USB device: `<vid>:<pid>`,
`<vid>:<pid>@<serial>`, or `port:<bus>-<port>[.<port>...]`. Resolves to
exactly one device or is refused — with the candidates listed when it matches
several. A detach resolves it against the VM's attachments, not the host.
_Avoid_: device id (that is the `vid:pid` pair alone), filter

### Templates and images

**Template**:
A sealed, read-only qcow2 disk image in the store, referenced by
`<arch>/<name>[@<version>][#<variant>]`. Labs boot linked clones of it. A
template version is one **base** and zero or more **variants**; one with none
is base-only and is its base.
_Avoid_: image (an image is an OCI container image), golden image

**Base**:
The one disk of a template version every variant is built on —
`<version>/disk.qcow2`. On a template with variants it is *unsealed* (a
Windows base is installed and updated but not generalized), so a lab machine
may not clone it: `#base` names it in build contexts only, a variant build or a
layered `from`. On a base-only template the base is the template, as it always
was. Its `sha256` is what a variant's `base_sha256` must match.
_Avoid_: parent, root image

**Variant**:
A named, sealed disk state of one template version, built from its base and
stored as a qcow2 overlay on it (`variants/<name>/disk.qcow2`, backed by
`../../disk.qcow2`). Selected with `#<name>`; a reference with no `#` selects
the **default variant** — the one marked `default = true`, or a lone one. Its
metadata is the base's overridden field by field (the **effective metadata**).
One whose `base_sha256` no longer matches the base is **stale** and refused
until rebuilt. See ADR-0017.
_Avoid_: snapshot (that is a VM snapshot, §7.3), flavour, edition, layer

**Linked clone**:
A copy-on-write qcow2 overlay a machine boots, backed by a template. The
template is never written to. Short form "clone" is fine.

**Store**:
The local template store. Every write goes through the supervisor's `store.*`
and `template.*` commands, so there is one implementation of each; the store's
own file lock is what serialises them. Reads are lock-free, and a daemon that
already holds the store open — a lab daemon binding a clone — still reads it
directly. See ADR-0010.
_Avoid_: cache, registry (a registry is remote)

**OCI artifact**:
How a template is stored in a registry: a non-runnable artifact whose qcow2 is
chunked into zstd layers.

**Layered build**:
A build whose source is an existing template rather than an ISO, a qcow2 or a
blank disk. Its working disk starts as a copy of the source's, and its hardware
starts from the source's recorded metadata (ADR-0009).
_Avoid_: derived template, child template

**Build lab**:
The synthetic one-VM lab a build runs as — rendered as WCL, `scratch`, with its
primary disk pre-seeded from the source. Having no template layer (§6.5), it
carries inherited hardware as vm-block attributes instead.

**Effective build hardware**:
What a build boots on and seals: the template block's declared hardware over the
source template's recorded hardware, merged once before the build lab is
rendered. It names a profile but takes nothing *from* one — no profile-derived
value is frozen into the build lab or the sealed image, so the profile stays a
live layer, resolved when a VM clones the sealed template, and profile edits
keep reaching existing templates (ADR-0009).

**Profile**:
A named set of hardware defaults — machine, firmware, TPM, disk bus, NIC,
display, CPUs, memory — chosen with `profile = "..."`. Both machine kinds
name one: a VM inherits VM block > template > profile, a container block >
profile (it has no template layer). One resolver applies that precedence for
both, and nothing else may reimplement it (ADR-0008).

### Guest automation

**wscript**:
vmlab's statically typed, Rust-flavoured scripting language for guest
automation. Compiled and type-checked at `vmlab validate` time.
_Avoid_: wisp (the former name — never use it)

**Provision**:
A wscript script run on `vmlab up`, and during template builds, to set a guest
up. A failure fails `vmlab up`.

**Scoped provision**:
A `provision {}` block declared inside the machine it configures: it runs once
that machine is ready and gates `depends_on`, so dependents wait for it.

**Playbook**:
A config-weave configuration folder bound to a machine, or to a template
build, with a `playbook {}` block. Applied on `up` interleaved with
provisions.
_Avoid_: recipe, role, manifest

**config-weave**:
The declarative guest-configuration system vmlab integrates for playbooks:
plays converge packages, files and services with drift detection, idempotent
re-runs and automatic reboots.

**vmlab-agent**:
vmlab's first-party in-guest agent, reached over a virtio-serial port with no
guest network involved. Powers readiness, exec, **file operations**
(`fileops`: handle-based, offset-addressed, pipelined — every transfer runs
over it), terminals, tail, metrics, clipboard and the workspace watch, in
both VMs and containers.
_Avoid_: QGA, qemu-guest-agent (removed), guest tools

**Progress watchdog**:
The rule that a `fileops` channel which goes silent fails rather than waits:
nothing from the guest for 60 s with a request outstanding fails the call,
naming the request and the file, and the channel with it. Measured as
silence, so a slow transfer that is still moving never trips it; a `digest`'s
budget grows with the file.
_Avoid_: timeout (reads as elapsed time, which a slow transfer would exceed)

**Port connection reset**:
The agent abandoning a virtio-serial port connection whose write pended too
long (15 s on Windows) and reopening the port. Bytes are lost in both
directions, so the agent fails every open channel with `channel lost: port
connection reset`, and its port writer offers a refused frame again instead
of dropping it.
_Avoid_: reconnect (the host never disconnected), hang

**Event handler**:
A wscript script bound with `on "event" {}` that reacts to a lifecycle event.
Failures are logged, never fatal.

### Runtime and orchestration

**Supervisor**:
The per-user daemon `vmlabd`, auto-started by the CLI. Owns the lab registry,
global segments, store writes and host watchdogs.
_Avoid_: daemon (ambiguous with the lab daemon), server, master

**Lab daemon**:
The per-lab daemon spawned by the supervisor on `vmlab up`. Owns the machines,
the network fabric, snapshots and the wscript runtime.

**Build**:
Which `vmlab` binary a process is: the crate version plus the git revision it
was built from (`0.31.0+1a2b3c4`). The CLI drives only daemons of its own
build and refuses any other by name, `vmlab daemon stop` being the fix; the
wire carries no protocol version, so the build is the compatibility unit.
_Avoid_: version (alone — two builds share one)

**Hypervisor**:
The seam between deciding a machine should run and the host actually running
it. Stated as what running means — the machine is up, it answers control, it
exited for this reason — rather than which binary is launched, and it hands
back its own handle types (**Process**, **Control**) rather than a host process
and a QMP client, so an adapter can be entirely in-memory. TPM, filesystem
daemons, the guest boot asset and process spawning sit below it; power state,
exit classification, readiness and teardown ordering sit above.
Two adapters, both live: QEMU in production, the fake in the lifecycle tests.
_Avoid_: driver, backend, runtime

**Spawner**:
The guest-side counterpart of the Hypervisor seam, inside vmlab-agent: the one
place a guest process or a written file handle is created. Every call takes an
**Identity** — who the work runs as — so terminals, exec and file pushes all
answer that question in one place rather than three. It hands back its own
**Spawned** handle (stdio, resize, kill, wait) rather than a child process or a
PTY, so an adapter can be entirely in-memory. Three adapters, all live: Linux
and Windows in production, the fake in the session tests.
_Avoid_: launcher, process factory, executor

**Adopter**:
The one thing the **Spawner** hands out that is not a handle: a resolved
identity a session thread can wear while it opens files of its own. Reads need
it because `tail` reopens across rotation, so a single handle would not carry
the identity far enough. Building the adopter mints the logon — which is where
a missing account or a wrong secret fails, loudly — and wearing it produces a
guard that lets go when dropped.
_Avoid_: impersonation (that is the Windows mechanism, not the concept),
context, credential

**Plan**:
A decision computed in full, as a value, before anything acts on it. Computing
the plan does no I/O; carrying it out is a separate operation. **LabPlan**,
**Share plan**, **Mount steps**, **Pull ledger**, **Forward plan** and the
workspace **Reconciliation** are the plans the lab daemon computes.
_Avoid_: strategy, intent, command object

**LabPlan**:
The ordered waves of machines, and the configuration steps between them, that
a given `up` or `down` will carry out — computed in full before anything is
started or stopped.
_Avoid_: schedule, DAG, dependency graph

**Share plan**:
Which shared folders ride virtiofs and which fall back to SMB, per segment,
together with the host port the bundled server takes.

**Mount steps**:
The ordered guest commands that mount a share plan inside a given guest OS.
Guest-side knowledge lives here, not in the lab daemon.

**Pull ledger**:
The lifecycle and progress of template and image downloads — pending, active,
done, errored, cancelled — as a value the CLI reads.

**Forward plan**:
The port-forward rules a lab's machines require, resolved to leases and
gateways before any is installed.

**Login**:
A labelled identity declared on a machine with a repeatable `login {}` block —
an account, its secret, and whether it is elevated. What a surface runs *as*,
selected by label — `--user`, `as_login`, or the **default login** — never by
the raw account. Declared on the machine rather than on `@dev`, because `exec`,
`shell` and `vmlab cp` reach every machine: an unmarked machine needs an
identity too. A machine may declare any number, or none — with none, everything falls to the
**agent identity**.
_Avoid_: user (that is a person, or the guest's OS account), credential (that
is the secret alone), account (the guest owns those; vmlab owns the login)

**Default login**:
The login carrying `default = true`, or — where none does — the only login the
machine declares. The same implicit-lone rule as the **default dev machine**,
and for the same reason: a lone declaration never has to meet the concept.

**Script identity**:
The wscript rung of the identity precedence ladder: `m.as_login(label)` /
`m.as_account(user, password)`, which resolve a **login** the same way
`--user`/`--password` do and hand back a *second handle onto the same machine*
rather than switching this one. So an identity is said once and every call on
the result — exec, file push, terminal — carries it. What makes a
`provision {}` step able to address a **login**'s home directory before that
account has ever logged on, and therefore the only route for anything that must
land as the developer rather than as the machine. A **playbook** has no rung
here at all.
_Avoid_: run-as, impersonation, sudo (none of the three is a mode switch)

**Declared placement**:
Per-user content — tools, plugins, settings — put into a guest home by a
template build or a lab `provision {}`, as opposed to hand-installed by a
developer from a shell. Both declared placements re-apply across a
per-machine `destroy` + `up` because a fresh clone is re-made and boots
first-boot again; a hand-install does not. Hence: bake what the lab needs every
developer to have, hand-install what you personally want today, and expect to
redo it after a rebuild. A per-machine durable home overlay was rejected — it
would reintroduce exactly the surviving guest-side state the **workspace**
retired a disk to eliminate, and declared content already has a canonical
durable home. See PRD §19.8.
_Avoid_: provisioning extensions (vmlab moves the bytes and never interprets
them), dotfiles (those are the developer's)

**Agent identity**:
What the guest agent itself runs as — SYSTEM on Windows, root on Linux. The
floor when no login applies, and what vmlab's own machinery uses on its own
behalf, *except where it produces the developer's files*. Spelled
`--user SYSTEM` / `--user root`.
_Avoid_: system user, service account

**Container floor**:
The **agent identity** inside a container micro-VM: the user cinit resolved for
the workload — the declared `user`, else the image's `USER`, else root — which
every session lands as when the container declares no **login**. It is
devcontainers' `remoteUser`/`containerUser`, and it costs nothing because Linux
needs no credential to become that user.

**Cached logon**:
The identity minted once per (account, secret, machine) and shared by every
channel using it, which is what makes "the file transfer's logon is the shell's
logon" true by construction. On Windows that is a token: one `LogonId`, one
ticket cache, one set of drive mappings, with the user's profile loaded when it
is minted and unloaded when it is dropped. On Linux the **login session** is
each `su`'s, so what is shared is the resolution — the account, the machinery,
the runtime directory. Dies with the machine, and is recycled at idle once
older than its Kerberos ticket lifetime.
_Avoid_: session (ambiguous with a login session or a terminal)

**Login session**:
What a **login** gets on a Linux guest: the environment, supplementary groups,
`XDG_RUNTIME_DIR`, cwd and login shell a real login would have — realised
through the guest's own `su`, so PAM runs, and assembled by the agent itself
where the guest has no PAM. Which of the two ran is named in the agent's log
and in the terminal's banner, because it is the answer to "why does rootless
podman not work in here".
_Avoid_: setuid (that is the fallback, not the concept), impersonation

**Fileops session**:
One agent channel serving file requests as an RPC session: handle-based,
offset-addressed, pipelined with out-of-order replies, records framed inside the
channel's own credit window. Opened per transfer by `vmlab cp`, wscript
push/pull and the workspace syncer; handles are scoped to the channel and die
with it.
_Avoid_: file channel (it carries requests, not one file's bytes)

**Diverged machine**:
A running machine whose guest content no longer matches the template it was
cloned from, because vmlab deliberately changed it in place — today, only by
**agent repair**: the verb, or the refresh `up` performs on a stale agent.
Divergence is always reported, and the refresh is the one case vmlab starts
without being asked, which `agent_update = false` turns off; the template's
sealed metadata is otherwise the truth about what a clone contains. Recorded in
the lab's own state and forgotten with the disks it lived on, so `destroy` +
`up` with `agent_update = false` is what puts a machine back on its sealed
agent.
_Avoid_: dirty, modified, patched, drifted (drift implies unnoticed)

**Workspace syncer**:
The lab-daemon-owned component that keeps a **workspace**'s two copies in step:
guest changes arrive through the agent's watch, host changes through the host's
own watcher, and every apply lands temp-then-rename. The one piece of vmlab's
machinery that runs as the machine's default login rather than the agent
identity, because its whole output is the developer's own tree.
_Avoid_: sync daemon, mirror, replication

**Sync ledger**:
The host-side record of last agreed state per path — content digest, plus each
side's own size and mtime as a pre-filter. Lives in the lab's `.vmlab/` per
(machine, workspace) and dies with `destroy`. Each side's mtime is compared only
against its own recorded value; a host mtime is never compared to a guest one.
_Avoid_: index, manifest, state file

**Agreed**:
A path whose two sides matched at a known point, as recorded in the sync ledger.
A **conflict** is a path where both sides moved since one.

**Windows preconditions**:
The three things vmlab actively does to a Windows workspace, because each is a
precondition of the *mechanism* rather than the lab author's `provision {}`: the
NTFS case-sensitivity flag on every directory the syncer creates, a symlink
attempted and warned about by name where it fails, and the guest's git
line-ending conversion turned off. Resolved as a value before the syncer's loop
starts.
_Avoid_: Windows fixups, quirks, workarounds

**Case collision refusal**:
Two host paths differing only in case, refused by name where the guest folds
case — the fallback to the case-sensitivity flag, never the policy. Neither copy
is transferred, because picking one is the silent overwrite the refusal exists
to prevent.
_Avoid_: case conflict (a **conflict** is two moved sides), case clash

**Workspace degradation**:
A named way this machine's workspace is less than whole, reported **up front**
rather than discovered at a random path. A Windows login declared
`elevated = false` brings exactly two: no case-sensitive directories and no
symlinks.
_Avoid_: limitation, unsupported, fallback mode

**Sync halt**:
The workspace-wide, both-directions stop a conflict triggers on one machine,
naming every conflicting path in the batch. Distinct from a **deferral**, which
is timing and clears itself with no developer action. Its paths are recorded on
the **sync ledger** as well as held in memory, so stopping the machine is not a
way to lose the refusal a **snapshot bracket** owes them.
_Avoid_: pause, error state, conflict mode

**Deferral**:
A path or a direction the syncer is leaving alone *for now*, with no developer
in the loop: the `.git` mutable set while a lock is held on either side, both
directions between an overflow and the completed rescan, and both directions
between a **snapshot bracket**'s rewind and the completed **re-seed**. It clears
itself. Never a **sync halt**, which needs an answer, and never a **skip**, which
is permanent for as long as its cause is.
_Avoid_: halt, blocked, stalled (all three read as needing a developer)

**Resolution**:
A developer's answer to a halted path: `--host` keeps the canonical copy,
`--guest` keeps the working copy, and making both sides identical by hand needs
no verb at all. **An input, not an act** — it is handed to the next
reconciliation, which turns that path into the ordinary one-sided propagation
the winner's own change would have been. Host-side necessarily: ADR-0013 leaves
no guest→host control path to offer it from.
_Avoid_: merge, fix, override

**Halt marker**:
The file a halt writes at the guest's workspace root, listing the halted paths.
In the **ignore floor**, so it never syncs. It exists because from inside the
guest a halt is otherwise *nothing happening*, on the one side no control path
reaches; its `git status` noise is the point.
_Avoid_: lock file, sentinel, status file

**Bulk-delete guard**:
The threshold — a proportion of the ledger, with a floor — past which
**guest→host** deletions are withheld and the workspace halts. Asymmetric on
purpose: the guest copy is reconstructible and the canonical one is not, so
host→guest deletes are unguarded. About mass, not about deletion: a single
deletion propagates immediately.
_Avoid_: delete protection, safety limit

**Volume warning**:
One pass carrying an unusual amount of work under one subtree, named with the
`.vmlabignore` rule that would make it guest-owned. **Warns and continues**,
always: a build burst is wanted work that happens to be large, and halting on it
would let a `cargo build` stop a dev machine. Distinct from the **size guard**,
which refuses.
_Avoid_: throttle, rate limit, overload

**Dirty set**:
The guest-side, deduplicated set of paths the agent's watcher has touched since
the last drain. A set, not a queue: it holds paths, not events, so no platform
event kind ever crosses the seam. Capped; exceeding the cap collapses the batch
to a rescan.

**Drain**:
The host swapping the dirty set out and receiving it as a batch of stat records,
each the path plus its current kind, size and mtime — or a tombstone. At most
one outstanding.

**Direction**:
Which way one reconciled action carries a change — to the guest, or to the
host. A property of the action rather than of the pass: **both directions are
the same matrix**, and only the deletion guards and the receiving side's own
record differ between them. Authoring happens guest-side, so *to the host* is
the busy direction rather than the exceptional one.
_Avoid_: way, mode, upload/download (nothing here is a transfer's own idea)

**Prune list**:
Host-computed directory prefixes the guest never registers a watch on — ignored
directories with no negation reaching below them. The host still decides; the
guest is handed the answer, never asked the question.

**Watch discontinuity**:
Any watch (re)open. Identical to the list of agreement-losing events — first
sync, ledger loss, overflow, post-restore re-converge — which is why no resync
token exists. Three of the four are answered by the **stat-walk**; the fourth is
answered by the **re-seed**, because vmlab performed the rewind and does not need
to ask what happened.

**Snapshot bracket**:
What vmlab does around a snapshot of a machine carrying a workspace, and the
argument that the syncer is vmlab-integrated rather than a generic tool wrapped:
an off-the-shelf one cannot know a rewind happened. **Capture** flushes first and
refuses — with no escape flag — where the guest holds work the canonical copy has
never seen. **Restore** takes the syncer off the workspace, rewinds, and puts it
back owing a **re-seed**; it refuses while a **sync halt** stands unless
`--discard-guest-changes` says otherwise, because it destroys the guest copy of
every conflicting path by design. **Snapshots are not a workspace backup**, and
every surface that takes or restores one says so.
_Avoid_: hook, wrapper, pre/post step

**Re-seed**:
The host-only, digest-based reconcile a **snapshot bracket**'s restore owes: the
guest tree is walked to decide what to overwrite and delete and for nothing else,
and no action it produces can flow guest→host, so a rolled-back copy cannot reach
the canonical one. **By digest, never by mtime** — a restored guest's clock runs
behind the host, so a same-size in-place write compares identical on
`(size, mtime)`. It completes **before the watch reopens**, and it replaces the
**stat-walk** rather than following one. That it is owed rides the **sync
ledger**, because a restore does not need a running machine.
_Avoid_: resync, re-sync, reseed pass, full push (a wipe-and-re-transfer is
legal, not required)

### Configuration and surfaces

**Schema projection**:
The single description of the `vmlab.wcl` schema — every block, field, type,
optionality, default, doc string, option list, nesting and cardinality, plus
every decorator the schema declares (where it may be written, how often, and
its typed arguments) —
**reflected** from `schema.wcl` rather than restated (`src/config/projection.rs`),
and read by everything that needs the schema's shape, the rendered reference
above all. See ADR-0005.
_Avoid_: DTO, view model, descriptor table

**Block extractor**:
The one module that turns a WCL block into a typed value — field access,
coercion, source spans and the issue vocabulary. Lab files, host config,
profiles and template metadata all read through it. `config::block`; its
per-block cursor is `Reader`, and what each caller keeps is a *field mapping*.
See ADR-0006.
_Avoid_: parser (wcl parses; this reads a parsed block), deserializer

**Host GPU**:
A GPU on this machine, declared by name in the user's host config
(`gpu "AMD1" { address = "0000:72:00.0" }`) with its PCI address and an
optional `vulkan_icd`; at most one is the default, and a lone one is the
default without saying so. A lab's `gpu {}` names one with `host_gpu`, or
takes the default — never an address, because which card sits where is a
fact about one machine and a lab file travels. Names match
case-insensitively. A paravirtual mode falls back to the default from a name
this host lacks; `passthrough` refuses. Picked by the lab daemon at VM start
(`qemu::hostgpu::pick`). A template's or variant's `gpu {}` is the build VM's
(`template::build::BuildSpec::gpu`), picked the same way and never sealed into
the metadata. PRD §5.2, §6.1.
_Avoid_: GPU address (the address is a host GPU's field), render node (the
node is what sysfs resolves the address to)

**PCI filter**:
vmlab's own implicit Vulkan layer, `VK_LAYER_VMLAB_pci_filter`, loaded into
every `vulkan` VM's QEMU on a picked host GPU. It hides every physical device
but the one at that host GPU's PCI address (`VK_EXT_pci_bus_info`), so a
Venus guest sees one device even where two identical cards both pass the
driver filter. Built into the binary (`qemu::vk_layer`), written to the data
directory at VM start. PRD §5.2.
_Avoid_: device select (Mesa's layer, which breaks Venus), driver filter (the
loader's `VK_LOADER_DRIVERS_SELECT`, which matches a driver, not a device)

**Lab status**:
The typed projection of a lab's machines, segments and pulls that the lab
daemon produces once and every surface renders unchanged. Carries the
state-to-label derivation, so every surface speaks one vocabulary. A VM's
entry records what its last start decided — the host GPU, its `qemu_binary`
and the environment vmlab set on QEMU (`qemu_env`) — not what the host
config says now. `vmlab status --json` prints it as it arrived. See
ADR-0004.
_Avoid_: state (a machine has a power state), snapshot, summary

**Request vocabulary**:
The enumeration of every command a daemon serves, each variant carrying that
command's argument shape. One per daemon — the supervisor's and a lab's — so
each dispatch is an exhaustive match. The command string is still what goes on
the wire; nothing above the protocol spells it. See ADR-0007 and the generated
`docs/protocol.md`.
_Avoid_: RPC, command table, API (an API is a surface, not the vocabulary
under it)

**Inline transfer**:
Carrying a file's bytes *in* a wire message rather than beside it:
`machine.push_file` with `data`, and `machine.pull_file` with no host path.
It is the only form available to a caller that holds bytes and no path the
daemon can see. Bounded by `proto::INLINE_FILE_LIMIT`,
which the transport's request cap is derived from, so exceeding it is refused
by code rather than truncated; the host-path forms stream and have no ceiling.
_Avoid_: upload/download (neither says which side holds the file), base64
transfer (the encoding is incidental)

**Error code**:
The machine-readable half of a failed reply, beside the human-readable message.
The code is the contract — it decides the CLI's exit code — and the message
is free to be reworded.
_Avoid_: error type, status (a status is a lab's or machine's condition)

**Surface**:
Something a person or a program drives vmlab through: the CLI, or one daemon
calling another. Each adapts the request vocabulary; none holds its own list
of commands. Which surface reaches which command is the coverage report in
`docs/protocol.md`.
_Avoid_: frontend, client (a client is the protocol's connection object)

**One-way command**:
A command the coverage report finds reachable from exactly one surface. Not
automatically a gap — several only mean anything from one place — so every one
declares which it is beside its declaration in the request vocabulary: a
deliberate one carries `#[one_way("surface", "why")]` and an open gap carries
`#[one_way_gap("surface", N)]`, naming the issue tracking whether it should
close. The report renders the two as separate lists, and the build fails while
a one-way command carries neither. See ADR-0007.
_Avoid_: dead command (that is one with no caller at all), CLI-only (name the
surface the report names)

### Networking and storage transports

**Fast path**:
Optional eBPF network acceleration above the userspace switch — the afxdp tier
chosen by `auto`, and the explicit-only sockmap tier. Any failure falls back
to userspace.

**virtiofs**:
The shared-filesystem transport that `share {}` blocks and container volumes
ride by default — no guest networking, snapshot-safe. SMB/CIFS is the fallback
transport.
