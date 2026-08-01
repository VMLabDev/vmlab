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
`lab {}` block in `vmlab.wcl`.
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

### Capabilities

**Capability**:
Something one machine can do that another might not — a display, a clipboard,
a Windows event log. Probed and reported, never inferred from whether the
machine is a VM or a container.

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
The local template store. Writes are serialised by the supervisor.
_Avoid_: cache, registry (a registry is remote)

**OCI artifact**:
How a template is stored in a registry: a non-runnable artifact whose qcow2 is
chunked into zstd layers.

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
guest network involved. Powers readiness, exec, file transfer, terminals,
tail, metrics and clipboard, in both VMs and containers.
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
exit classification, readiness, teardown ordering and restart policy sit above.
Two adapters, both live: QEMU in production, the fake in the lifecycle tests.
_Avoid_: driver, backend, runtime

**Plan**:
A decision computed in full, as a value, before anything acts on it. Computing
the plan does no I/O; carrying it out is a separate operation. **LabPlan**,
**Share plan**, **Mount steps**, **Pull ledger** and **Forward plan** are the
plans the lab daemon computes.
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

### Configuration and surfaces

**Schema projection**:
The single description of the `vmlab.wcl` schema — every block, field, type,
default and doc string — **reflected** from `schema.wcl` rather than restated,
and read by everything that needs the schema's shape: the designer's forms, the
console's config types, the enum option lists, the rendered reference. See
ADR-0005; until it lands, the designer is still driven by hand-written
descriptor tables.
_Avoid_: DTO, view model, descriptor table

**Block extractor**:
The one module that turns a WCL block into a typed value — field access,
coercion, source spans and the issue vocabulary. Lab files, host config,
profiles and template metadata all read through it. See ADR-0006.

**Lab status**:
The typed projection of a lab's machines, segments and pulls that the lab
daemon produces once and the CLI, REST surface and web console each render
unchanged. Carries the state-to-label derivation, so all three surfaces speak
one vocabulary. See ADR-0004.
_Avoid_: state (a machine has a power state), snapshot, summary

**Web console**:
The browser UI served by `vmlab-web`: lab overview, visual designer, file and
log editors, per-machine consoles and terminals, template builds, playbooks,
and proxied guest web pages.
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
