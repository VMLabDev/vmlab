# vmlab

A single-host VM lab orchestrator: labs and virtual networks declared in WCL,
reusable disk templates built locally or distributed over OCI registries, and
guest automation written in wscript.

This is the canonical glossary. `docs/wskills/vmlab/data/reference/glossary.wcl`
restates the user-facing subset for the rendered wskill and site; when the two
disagree, this file wins.

## Language

### Labs and machines

**Lab**:
A set of machines plus the virtual networks connecting them, declared in a
`lab {}` block in `vmlab.wcl`. Its declared name identifies it uniquely among
the labs registered on one host.
_Avoid_: environment, project, stack

**Machine**:
Anything a lab boots, attaches to a segment, and drives through the agent — a
VM or a container. The unit that `depends_on`, `up` waves and `machine.*`
operations address.
_Avoid_: node, instance, workload

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
on its block, which vmlab publishes as an SSH endpoint an editor attaches
*into*. VM or container, Windows or Linux — one contract for every machine
kind. The decorator states something *about* the machine rather than
configuring something *inside* it: nothing it carries is a setting the guest
sees. A lab may have any number, or none.
_Avoid_: devbox, workspace (a workspace is the source tree on one), devcontainer

**Default dev machine**:
The dev machine carrying `@dev(default = true)`, or the only one carrying
`@dev`. A property of the **lab file**, so it is the same for everyone who
opens it — not per-developer. Which dev machine is *mine* is host-side state
and deliberately not expressible in `vmlab.wcl`.

**Dev selection**:
Which dev machine is *mine* — one developer's answer, recorded by `vmlab dev
use` in the lab's own gitignored `.vmlab/`, and forgotten with it by `destroy`.
Per-developer by construction, which is exactly what the committed lab file
cannot express, and keyless because it lives inside the lab it describes. It
is one rung of a fixed ladder — argument, `VMLAB_DEV_MACHINE`, selection,
**default dev machine** — every rung of which is checked rather than trusted:
a rung naming a machine this lab does not offer is an error at that rung, never
a fall through to the next. See PRD §19.7.
_Avoid_: current dev machine, active machine, context (nothing is switched)

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
One reconciliation of a workspace end to end — walk the host, ask the guest
about those paths, reconcile against the **sync ledger**, apply, record. The
**seed** is simply the first one; there is no separate seeding mechanism.
_Avoid_: sync cycle, run, tick

### Capabilities

**Capability**:
Something one machine can do that another might not — a display, a clipboard,
a Windows event log. Probed and reported, never inferred from whether the
machine is a VM or a container.

**Attachable**:
A machine whose agent serves both `tunnel` and `fileops` — *this agent can
serve an attach*, never *your attach will succeed*, since identity is declared
separately. A **capability** computed over probed features, reported by
`vmlab machine capabilities` and carried in **lab status**; deliberately not
widened to `watch`, which is the **workspace syncer**'s different question.
Where it is false, `validate` says nothing, `up` warns, and an attach fails —
naming the rebuild and the **agent repair** verb.
_Avoid_: attachable-ready, sshable, dev-ready

**Agent repair**:
Pushing the host's shipped agent binary into a running machine over the
agent's own channel, replacing what its artefact baked. A tool, never a
policy: it fires only when someone types it, because an automatic refresh
would make a template's sealed `agent_version` a lie, and it makes the machine
a **diverged machine**. Meaningless for a machine whose agent came with the
host rather than with what it boots — a container micro-VM's — which is
reported rather than implied.
_Avoid_: update, upgrade, hot-patch, self-update

**Display**:
A machine's framebuffer, together with the keyboard, pointer, OCR and
image-matching operations that read and drive it. Probed and reported like any
other capability — not inferred from machine kind. No container reports one
today; one running a display server could, so absence is reported as "this
machine has no display", never as "containers cannot have displays".
_Avoid_: console, screen, VNC, framebuffer (alone)

### Templates and images

**Template**:
A sealed, read-only qcow2 disk image in the store, referenced by
`<arch>/<name>[@<version>]`. Labs boot linked clones of it.
_Avoid_: image (an image is an OCI container image), base, golden image

**Linked clone**:
A copy-on-write qcow2 overlay a machine boots, backed by a template. The
template is never written to. Short form "clone" is fine.

**Store**:
The local template store. Every write goes through the supervisor's `store.*`
and `template.*` commands, so there is one implementation of each; the store's
own file lock is what serialises them. Reads are lock-free, and a daemon that
already holds the store open — a lab daemon binding a clone, the web process
listing versions — still reads it directly. See ADR-0010.
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
over it), terminals, tail, metrics, clipboard and **tunnels**, in both VMs
and containers.
_Avoid_: QGA, qemu-guest-agent (removed), guest tools

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
done, errored, cancelled — as a value the console and CLI both read.

**Forward plan**:
The port-forward rules a lab's machines require, resolved to leases and
gateways before any is installed.

**SSH facade**:
The SSH protocol vmlab terminates on the host, reached as a stdio
`ProxyCommand` so nothing listens and no port is leased. Presents an SSH
interface with no sshd in the guest: `session` channels are serviced by
vmlab-agent, SFTP is terminated host-side over a **fileops session**, and
`direct-tcpip` rides an SSH-scoped tunnel stream. It only ever *answers* a
channel open, never initiates one (ADR-0013), which is why `-R`, agent
forwarding and X11 are refused. See PRD §19.3 and ADR-0012. It degrades **per
channel**: a machine whose agent cannot serve an attach still serves a shell,
and only what needs the missing feature is refused, by name (§19.4).
_Avoid_: sshd, SSH server (implies guest-side), gateway, proxy

**Managed block**:
The marker-fenced region vmlab owns inside the developer's own
`~/.ssh/config` — its whole host-side footprint, and the only file it writes
outside its own directories. Deterministically ordered, refreshed by any
command that loads a lab, written only on a real difference, pruned by lab
root, and re-hoisted to the top of the file on every write so OpenSSH's
first-value-wins rule keeps it in effect. There is no vmlab-owned config file
and no `Include`: a client that cannot follow one is the reason. See PRD
§19.7.
_Avoid_: ssh config file (that is the developer's), include file, snippet

**Alias**:
One `Host` entry in the managed block: `vmlab-<lab>-<machine>`, plus
`vmlab-<lab>-<machine>-<label>` for each non-default **login**. Covers
*declared* machines, not running ones — it means "this machine exists in this
lab", never "it is attachable right now". `<lab>/<machine>` is the argument
form `vmlab ssh` and `ssh-proxy` take, and is disqualified as an alias because
the slash would land in the mux socket path.
_Avoid_: host entry, hostname (the stanza sets no `HostName`)

**Host key**:
The SSH identity the facade presents, minted per (lab, machine) into vmlab's
own state directory beside a `known_hosts` vmlab also owns. It survives
`destroy`, so a recreated machine presents the identity its entry already
records, and the developer's `~/.ssh/known_hosts` is never touched. No guest
holds one, so a template clone cannot carry a stale key and a snapshot restore
cannot roll one back.
_Avoid_: server key, machine key (that is not what it identifies)

**Login**:
A labelled identity declared on a machine with a repeatable `login {}` block —
an account, its secret, and whether it is elevated. What a surface attaches
*as*, selected by label; the SSH username carries the label, never the raw
account. Declared on the machine rather than on the attach, because the SSH
facade is a general capability: an unmarked machine needs an identity too. A
machine may declare any number, or none — with none, everything falls to the
**agent identity**.
_Avoid_: user (that is a person, or the guest's OS account), credential (that
is the secret alone), account (the guest owns those; vmlab owns the login)

**Default login**:
The login carrying `default = true`, or — where none does — the only login the
machine declares. The same implicit-lone rule as the **default dev machine**,
and for the same reason: a lone declaration never has to meet the concept.

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
_Avoid_: session (ambiguous with an SSH session or a terminal)

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
channel's own credit window. Opened per SFTP session by the SSH facade and per
transfer by the console and the workspace syncer; handles are scoped to the
channel and die with it.
_Avoid_: file channel (it carries requests, not one file's bytes), SFTP channel
(SFTP is terminated host-side and never reaches the guest)

**Tunnel**:
One agent channel carrying a TCP connection the agent dialled *inside* the
guest. The destination string crosses verbatim and the guest resolves it, and
no destination policy applies — any address the guest can reach. Opened only by
the SSH facade, for `direct-tcpip`. A dial that fails is a **connect failure**,
reported apart from a refusal so a SOCKS client can tell "nothing is listening"
from "vmlab refused you".
_Avoid_: port forward (that is the Forward plan's host→guest lease), socket,
proxy

**Diverged machine**:
A running machine whose guest content no longer matches the template it was
cloned from, because a vmlab verb deliberately changed it in place — today, only
the **agent repair** verb. Divergence is always user-initiated and always
reported; vmlab never diverges a machine on its own, because the template's
sealed metadata is otherwise the truth about what a clone contains. Recorded in
the lab's own state and forgotten with the disks it lived on, so `destroy` +
`up` is what puts a machine back on its sealed agent.
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

**Sync halt**:
The workspace-wide, both-directions stop a conflict triggers on one machine,
naming every conflicting path in the batch. Distinct from a **deferral** — the
`.git` lock wait and the post-overflow rescan barrier — which is timing and
clears itself with no developer action.
_Avoid_: pause, error state, conflict mode

**Dirty set**:
The guest-side, deduplicated set of paths the agent's watcher has touched since
the last drain. A set, not a queue: it holds paths, not events, so no platform
event kind ever crosses the seam. Capped; exceeding the cap collapses the batch
to a rescan.

**Drain**:
The host swapping the dirty set out and receiving it as a batch of stat records,
each the path plus its current kind, size and mtime — or a tombstone. At most
one outstanding.

**Prune list**:
Host-computed directory prefixes the guest never registers a watch on — ignored
directories with no negation reaching below them. The host still decides; the
guest is handed the answer, never asked the question.

**Watch discontinuity**:
Any watch (re)open. Identical to the list of stat-walk triggers — first sync,
ledger loss, overflow, post-restore re-converge — which is why no resync token
exists.

### Configuration and surfaces

**Schema projection**:
The single description of the `vmlab.wcl` schema — every block, field, type,
optionality, default, doc string, option list, nesting and cardinality, plus
every decorator the schema declares (where it may be written, how often, and
its typed arguments) —
**reflected** from `schema.wcl` rather than restated (`src/config/projection.rs`),
and read by everything that needs the schema's shape: the designer's forms
(`src/config/designer.rs`, rendered into `web-ui/src/editor/schema.gen.ts`), the
console's pickers (`/api/catalog/meta`), and the rendered reference. The
console's configuration types still mirror the DTO rather than the projection;
now that the **Block extractor** has landed, that is the next slice. See
ADR-0005.
_Avoid_: DTO, view model, descriptor table

**Block extractor**:
The one module that turns a WCL block into a typed value — field access,
coercion, source spans and the issue vocabulary. Lab files, host config,
profiles and template metadata all read through it. `config::block`; its
per-block cursor is `Reader`, and what each caller keeps is a *field mapping*.
See ADR-0006.
_Avoid_: parser (wcl parses; this reads a parsed block), deserializer

**Lab status**:
The typed projection of a lab's machines, segments and pulls that the lab
daemon produces once and the CLI, REST surface and web console each render
unchanged. Carries the state-to-label derivation, so all three surfaces speak
one vocabulary. See ADR-0004.
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
daemon can see — a browser, above all. Bounded by `proto::INLINE_FILE_LIMIT`,
which the transport's request cap is derived from, so exceeding it is refused
by code rather than truncated; the host-path forms stream and have no ceiling.
_Avoid_: upload/download (neither says which side holds the file), base64
transfer (the encoding is incidental)

**Error code**:
The machine-readable half of a failed reply, beside the human-readable message.
The code is the contract — it decides the REST surface's HTTP status and the
CLI's exit code — and the message is free to be reworded.
_Avoid_: error type, status (a status is a lab's or machine's condition)

**Surface**:
Something a person or a program drives vmlab through: the CLI, the REST API,
the web console. Each adapts the request vocabulary; none holds its own list of
commands. Which surface reaches which command is the coverage report in
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
_Avoid_: dead command (that is one with no caller at all), CLI-only /
console-only (name the surface the report names)

**Web console**:
The browser UI served by `vmlab-web`: lab overview, visual designer, file and
log editors, per-machine consoles, terminals and guest file transfer, template
builds, playbooks, and proxied guest web pages.
_Avoid_: dashboard, portal, frontend

**Guest web page**:
An HTTP UI served inside a guest and declared with a `web {}` block; the web
console proxies it into a sandboxed iframe tab.

### Networking and storage transports

**Fast path**:
Optional eBPF network acceleration above the userspace switch — the afxdp tier
chosen by `auto`, and the explicit-only sockmap tier. Any failure falls back
to userspace.

**virtiofs**:
The shared-filesystem transport that `share {}` blocks and container volumes
ride by default — no guest networking, snapshot-safe. SMB/CIFS is the fallback
transport.
