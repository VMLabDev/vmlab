# vmlab — Product Requirements Document

**Status:** Draft v1
**Date:** 2026-06-12
**Depends on:** WCL (spec complete, implemented), wscript (assumed complete before this PRD is executed)

---

## 1. Overview

vmlab is a single-host virtual machine lab orchestrator. It defines **labs** — named groups of VMs and virtual networks — declaratively in WCL, builds and manages reusable **templates** (the Vagrant-box analogue), and drives lab automation through **wscript scripts** that can interact with guests at every level: power state, snapshots, keystrokes and mouse input, screenshot capture with image matching and OCR, and command execution and file transfer via the QEMU guest agent.

A two-tier daemon system owns all runtime state: a per-user **supervisor** manages lab lifecycle, the template store, and cross-lab/cross-host networking, and spawns one **lab daemon** per running lab that owns that lab's QEMU processes, network fabric, and state — so labs are fault- and contention-isolated from each other. The CLI is a client of both tiers; wscript scripts are written against a clean lab/VM API and are never aware of the daemons' existence.

vmlab targets QEMU/KVM exclusively, driven directly over QMP — no libvirt. Hosts are Linux, with **WSL2 explicitly supported as a first-class host environment**, which constrains the networking design (see §9).

### 1.1 Goals

- Reproducible multi-VM labs defined in a single `vmlab.wcl` file, validated by WCL schema before anything touches QEMU.
- Template building with the same automation machinery used for provisioning — boot an installer ISO, drive it with keystrokes and screen matching, seal, store.
- A scripting surface (wscript) rich enough to fully automate guests that have no automation hooks of their own — i.e. screen-driven automation as a first-class capability, not a fallback.
- A self-contained virtual network stack — switching, DHCP, DNS, routing, NAT, port forwarding, traffic filtering and redirection — that requires no root privileges and no host network configuration, and works identically on bare Linux and WSL2.
- Sensible zero-config defaults: a lab with no network declarations still gets working networking, addressing, and name resolution.

### 1.2 Non-goals

- **Performance.** The userspace network fabric will not approach tap/bridge throughput. Acceptable for v1; vhost-user or tap backends are future optimisations.
- **Multi-host scheduling.** vmlab runs VMs on one host. The only cross-host feature is attaching a network segment to a peer daemon (§9.4); placing or migrating VMs across hosts is out of scope.
- **Security isolation / multi-tenancy.** vmlab is a lab tool for a single trusted user. It is not a security boundary and makes no hardening claims.
- **libvirt compatibility.** No domain XML, no virsh interop.
- **Hypervisors other than QEMU/KVM.**

---

## 2. Concepts

| Term | Definition |
|---|---|
| **Lab** | A named group of VMs and segments defined in one `vmlab.wcl`, brought up and torn down as a unit. |
| **Template** | A reusable, sealed base image (qcow2 + metadata) stored in the local template store, keyed by `arch + name + version`. The Vagrant-box analogue. |
| **VM** | An instance in a lab, created as a linked clone (qcow2 backing file) of a template. Disposable by default. |
| **Segment** | A named L2 network. Implemented as a virtual switch inside its owning daemon — the lab daemon for lab segments, the supervisor for global ones. Per-lab by default; declarable as `global` to span labs (and hosts). |
| **Provision script** | A wscript script listed in `vmlab.wcl`, run during `vmlab up` after its VMs are ready. Receives a lab handle. |
| **Handler** | A wscript function bound to a daemon event (lifecycle, error, disk-space) for a lab or VM. |
| **Guest OS profile** | A named bundle of hardware defaults (firmware, machine type, devices) applied to a VM or template. |
| **Ready** | A VM is *ready* when its QEMU guest agent responds. A lab is *up* when all VMs are ready and all provision scripts have completed. |

---

## 3. Architecture

vmlab is a two-tier daemon system: one **supervisor** per user, one **lab daemon** per running lab.

```
┌─────────────┐ discover  ┌─────────────────────────────────────────┐
│  vmlab CLI  │ ────────► │          vmlabd (supervisor)            │
└──────┬──────┘           │  lab lifecycle · lab registry           │
       │                  │  global segments · cross-host peering   │
       │ direct           │  template store writes · host watchdogs │
       │ (lab ops)        │  event aggregation                      │
       ▼                  └───────────────┬─────────────────────────┘
┌──────────────────────────────┐          │ spawn/reap · segment trunks
│   lab daemon (one per lab)   │ ◄────────┴──────┐
│ ┌──────────┐ ┌─────────────┐ │   ┌─────────────┴────────────────┐
│ │lab state │ │ net fabric  │ │   │  lab daemon (another lab)    │
│ │ manager  │ │ switch·DHCP │ │   └──────────────────────────────┘
│ └────┬─────┘ │ DNS·NAT·etc │ │
│      │ QMP   └──────┬──────┘ │
└──────┼──────────────┼────────┘
       ▼              ▼ unix socket netdevs
 ┌──────────┐   ┌──────────┐
 │ QEMU VM  │...│ QEMU VM  │
 └──────────┘   └──────────┘
```

- **Supervisor (`vmlabd`).** One per user, auto-started by the CLI. Owns: spawning a lab daemon on `up` and reaping it on `down`/`destroy`; the registry of running labs (name → socket, pid, state); **global segments** (§9.2) — shared switches live here, with lab daemons attached as trunk ports; cross-host peering; serialised writes to the template store (pulls, builds, imports — so concurrent labs can't corrupt it; reads are lock-free); host-level watchdogs (`host.disk_low`); and an aggregated event stream across all labs. If a lab daemon dies unexpectedly, the supervisor detects it, emits `lab.daemon_crashed`, and marks the lab failed — it does not silently restart it.
- **Lab daemon.** One per running lab, owning everything lab-scoped: that lab's QEMU processes, QMP and guest-agent channels, lab-local segments and their DHCP/DNS/routing/rules, clones and snapshots, lab state, and lab events (forwarded up to the supervisor's aggregate stream). A lab daemon's failure is contained to its lab; other labs and the supervisor are unaffected.
- **CLI.** Connects to the supervisor for discovery and host-scoped verbs (template store, `status` across labs, daemon control), then talks **directly** to the relevant lab daemon's socket for lab-scoped operations — no proxying in the hot path.
- **wscript runtime.** Executes **inside the lab daemon** — it must react to events and co-locating it with the lab's state and event stream keeps everything in one place. The script-facing contract is unchanged: scripts get the lab/VM API (§10) and remain daemon-unaware.
- **QEMU.** One process per VM, launched by its lab daemon with `-qmp`, a guest-agent virtio-serial channel, VNC display, and one stream-socket netdev per NIC into the lab daemon's switch.

### 3.1 Sockets and protocols

All control sockets are unix domain sockets under `$XDG_RUNTIME_DIR/vmlab/`: `vmlabd.sock` for the supervisor, `labs/<lab>/control.sock` per lab daemon (plus per-VM QMP/agent/NIC/VNC sockets in the same lab directory). The CLI↔daemon wire protocol (framing, request/response/event shapes) is an implementation detail but must support request/response commands, a subscribable event stream, and streamed output for long operations (template builds, provision runs). Supervisor↔lab-daemon control uses the same protocol.

**Segment trunks.** A lab daemon attaches to a supervisor-hosted global segment over a frame-forwarding trunk connection (unix socket locally). The **same trunk protocol over TCP** is what connects two supervisors for cross-host segments (§9.2) — one mechanism, two transports.

## 4. File and directory layout

| Path | Contents |
|---|---|
| `<repo>/vmlab.wcl` | The lab definition. Located by walking up from cwd, like git. |
| `<repo>/.vmlab/` | Lab-local working data: linked-clone qcow2s, snapshot data, built floppy/ISO images, screenshots, downloaded artefacts cache. Safe to delete when the lab is down. Should be gitignored. |
| `~/.local/share/vmlab/templates/<arch>/<name>/<version>/` | Template store: sealed qcow2 + `template.wcl` metadata. |
| `~/.local/state/vmlab/` | Daemon state, per-lab and per-VM logs (JSON lines), event history. |
| `$XDG_RUNTIME_DIR/vmlab/` | `vmlabd.sock` (supervisor) and `labs/<lab>/` directories holding each lab daemon's control socket and per-VM QMP/agent/NIC/VNC sockets. |

All XDG paths respect the corresponding environment variables.

---

## 5. Configuration model (`vmlab.wcl`)

> **⚠ Binding note — syntax.** All WCL fragments in this document sketch *intent only*. The exact syntax — block forms, attribute names where they collide with WCL conventions, schema declarations — must follow the WCL spec and its native schema system. The implementer should treat the semantics described here as the contract and derive the surface from the WCL spec. The same applies to every wscript fragment: the API binds to the wscript spec's actual function/type/module syntax.

A lab file declares, at minimum, a lab name and one or more VMs. Everything else has defaults.

Illustrative sketch:

```
lab "ad-lab" {

  segment "corp" {
    subnet  = "10.50.0.0/24"          # optional — auto-allocated if omitted
    dns     { server = "10.50.0.10" } # hand out the DC as DNS instead of the daemon
    routes  { "10.60.0.0/24" via "10.50.0.254" }
  }

  segment "dmz" { }                   # zero-config: auto subnet, daemon DHCP+DNS

  vm "dc01" {
    template = "x86_64/windows-server-2025"   # arch required; latest version
    profile  = "windows-server"       # usually inherited from template
    cpus     = 4
    memory   = 8GiB

    nic { segment = "corp"  ip = "10.50.0.10" }   # static → DHCP reservation
  }

  vm "client01" {
    template   = "x86_64/windows-11@26100.1"  # version pin
    depends_on = ["dc01"]
    nic { segment = "corp" }           # dynamic lease
  }

  vm "buildbox"  {
    template = "x86_64/linux-modern"
    nic { nat = true }                 # internet egress only, no segment to declare
  }

  vm "airgapped" { template = "x86_64/windows-11" }    # no nic blocks = no network at all

  vm "installtest" {
    template = "scratch"               # no backing image: blank disk, OS install testing
    arch     = "x86_64"
    profile  = "windows-11"
    disk     = 80GiB
    cdrom    = "./isos/win11-build.iso"
  }

  vm "router" {
    template = "aarch64/linux-router@1.2"     # full arch/name@version form
    nic { segment = "corp" ip = "10.50.0.254" }
    nic { segment = "dmz" }
  }

  provision "scripts/setup.ws" { }            # runs on `vmlab up`, in listed order

  on "vm.crashed"    run "scripts/collect-dumps.ws"
  on "host.disk_low" run "scripts/alert.ws"
}
```

### 5.1 Validation

`vmlab validate` (and implicitly every other verb) evaluates the lab file against the vmlab WCL schema and fails before any side effect on errors including: unknown attributes, missing templates, undeclared segment references, static IPs outside their segment's subnet, duplicate static IPs/MACs, dependency cycles in `depends_on`, missing script files, archless or malformed template references, `scratch` VMs missing `arch`/`profile`/`disk`, and wscript compilation errors in all referenced scripts. The goal mirrors Config Weave: validation catches everything that can be caught without touching QEMU.

### 5.2 VM hardware surface

Each VM block can express:

- `cpus`, `memory`, `template`, `profile`
- Additional disks (size, optionally pre-formatted from a folder — see §6.3)
- CD-ROM and floppy attachments (paths or `media {}` blocks built from folders)
- `share {}` blocks — host↔guest shared folders (§7.5): host path, guest mount path, optional `readonly = true`
- Multiple `nic {}` blocks — segment, optional static IP, optional fixed MAC (generated and persisted otherwise), optional `isolated = true` for port isolation (§9.1). **A VM with no `nic {}` blocks gets no network hardware at all** — air-gapped is the default, connectivity is always explicit. `nic { nat = true }` is the shorthand for internet-only access (§9.7).
- `nested = true` — enables nested virtualisation (host CPU passthrough + the relevant accelerator flags)
- `gpu {}` — GPU acceleration, with a `mode` selecting between:
  - `passthrough` — full VFIO passthrough by host PCI address. Exclusive: the device leaves the host for the VM's lifetime.
  - `virgl` — paravirtualised OpenGL (virtio-gpu-gl + virglrenderer): the guest's GL is rendered on the host GPU, which stays shared — multiple VMs can accelerate at once. Requires guest virtio-gpu drivers (mature on Linux; Windows guest 3D support for virtio-gpu is limited and should be documented honestly rather than promised).
  - `vulkan` — paravirtualised Vulkan via virtio-gpu Venus. Newer and less settled than virgl; offered with the same guest-support caveats.

  The paravirtualised modes must coexist with vmlab's headless VNC model — host-side rendering with the framebuffer scraped for VNC/screenshots (QEMU's egl-headless-style display path). **⚠ Implementation note:** exact device/display flag combinations for virgl/Venus alongside VNC, and their behaviour on WSL2's GPU stack, change across QEMU versions and must be verified at implementation time rather than taken from this document. Screenshot/image-matching APIs (§10.3) must keep working in all GPU modes.
- `display`, `firmware`, `tpm`, `secure_boot` — normally supplied by the profile, overridable per VM
- `qemu_args = [...]` — **escape hatch**: raw arguments appended verbatim to the QEMU command line, last so they win

Values not set on the VM inherit from the template's recorded hardware; values not set there come from the profile; the profile's defaults are the floor. Precedence: **VM block > template > profile** (no template layer for `scratch` VMs, §6.5).

### 5.3 Guest OS profiles

Profiles bundle known-good hardware defaults. Starter set (final list and exact defaults to be confirmed against current QEMU/OVMF behaviour at implementation time):

| Profile | Machine | Firmware | TPM | Default devices |
|---|---|---|---|---|
| `windows-11` | q35 | OVMF + secure boot | swtpm 2.0 | virtio disk/net (with driver media support during template build), QXL or virtio-gpu, virtio-serial agent channel |
| `windows-server` | q35 | OVMF | swtpm 2.0 | as above |
| `windows-legacy` | i440fx or q35 | SeaBIOS | none | IDE/SATA disk, e1000 NIC, std VGA — for XP/7/2008-era guests with no virtio drivers |
| `linux-modern` | q35 | OVMF | none | virtio everything |
| `linux-generic` | q35 | SeaBIOS | none | virtio disk/net, conservative elsewhere — older or unusual distros |
| `custom` | nothing assumed | — | — | user supplies everything via VM/template attributes and `qemu_args` |

Profiles are data, not code: shipped as WCL, user-overridable and user-extensible from a profiles directory in XDG config.

---

## 6. Templates

### 6.1 Template definition and build

Templates are defined in `template {}` blocks — in a dedicated WCL file or alongside a lab — and built with `vmlab template build`. A template block specifies:

- **Source** — one of:
  - `iso` — installer ISO, local path or URL + required hash (`sha256 = "..."`). URL artefacts are downloaded to a cache and verified before use.
  - `qcow2` — existing disk image, local path or URL + hash. Imported as the base.
  - another template (`from = "<arch>/<name>@<version>"`) — layered builds: take an existing template, run more provisioning, seal as a new template.
  - `scratch` (§6.5) — blank disk; the build's attached installer media and provision script do everything.
- **Hardware** — disk size, profile, and any §5.2 attributes; these are recorded into the template's metadata and become the inheritance layer for VMs.
- **Media** — additional ISO/floppy attachments for the build, including images built from folders (§6.3) — unattend files, driver media, agent installers.
- **Provision scripts** — the same wscript machinery as labs (§10): the build boots the source, the script drives the installer with keystrokes/screen matching, installs the guest agent, configures, and seals.

Build flow: create working qcow2 → boot per template hardware → run build provision scripts → graceful shutdown → move qcow2 + metadata into the store under `<arch>/<name>/<version>/`. A failed build leaves nothing in the store.

### 6.2 Store, addressing, export

- Store key is **arch + name + version**. References take the form **`<arch>/<name>[@<version>]`** — arch is mandatory, always explicit, never inferred from the host; version omitted means highest in the store. `vmlab validate` rejects archless references.
- `vmlab template list / rm` manage the store.
- `vmlab template export` produces a single portable archive (qcow2 + metadata); `vmlab template import` installs one — the offline/sneakernet sharing path.
- The online sharing path is OCI registries (§6.4).

### 6.3 Media building

`vmlab` can build **ISO and floppy images from folders on disk**, declared inline in template/VM blocks (`media { type = "iso" from = "./unattend/" }`). Built images land in `.vmlab/` and are content-addressed so unchanged folders don't rebuild. Primary use: unattend/answer files, driver bundles, agent installers, payload delivery to guests with no network.

### 6.4 OCI registry distribution

Templates are distributable through standard OCI registries (GHCR, Docker Hub, Harbor, a self-hosted registry on Hermes — anything speaking the OCI distribution API), as **OCI artifacts, not container images**:

- **Artifact identity.** The manifest carries a vmlab-specific `artifactType` (e.g. `application/vnd.vmlab.template.v1`), and all blobs use vmlab media types. A `docker pull`/`docker run` against a vmlab reference must fail fast as "not a container image" rather than half-work — that's the whole point of typing it. Conversely, `vmlab template pull` refuses manifests that aren't vmlab artifacts.
- **Layout.** Config blob = template metadata (the recorded hardware, profile, agent info from `template.wcl`). Layers = the qcow2, **chunked**.
- **Chunking.** The qcow2 is split into fixed-size chunks — **default 512 MiB, configurable** — each pushed as one ordered layer blob with a chunk media type, compressed (zstd). Manifest annotations record chunk count, chunk size, total size, and the digest of the assembled image; pull reassembles in order and verifies the whole-image digest before installing to the store. Sizing rationale: GHCR (the expected primary home for templates) enforces a 10 GB per-layer limit *and* a 10-minute per-upload timeout — the timeout, not the size cap, is the binding constraint on realistic upstream bandwidth, and 512 MiB clears it with wide margin while keeping parallel transfer and chunk-granularity retry/resume cheap.
- **Multi-arch.** A registry tag may resolve through an **OCI image index** keyed by platform arch — mapping the store's `arch` dimension onto OCI's native multi-platform mechanism. Consistent with §6.2, arch is always explicit: `pull` requires `--arch` (or an unambiguous single-arch manifest) and never silently assumes the host arch.
- **Addressing.** `vmlab template push/pull ghcr.io/<owner>/<name>:<version>` — registry tag = template version. Pulled templates land in the local store under their arch+name+version like any other; the originating reference is recorded in metadata.
- **Lab references.** A lab's `template =` may be a registry reference with an accompanying explicit `arch`; `vmlab up` pulls it if absent from the store (and never re-pulls implicitly when present — updates are explicit via `pull`).
- **Auth.** Standard registry authentication, reusing existing Docker-style credential configuration/helpers where present so `ghcr.io` logins already on the machine just work. `vmlab template login` provided for standalone setups.


### 6.5 The `scratch` template

`template = "scratch"` is a reserved pseudo-template meaning **no backing image**: the VM gets a freshly created blank qcow2 instead of a linked clone, and there is no template layer in the hardware inheritance chain (precedence collapses to VM block > profile). Intended for VMs that should start with no OS at all — testing OS builds, installer development, bare-metal-style experiments.

Because nothing is inherited or fetched, validation requires three things a normal template would otherwise supply: an explicit `arch` (which selects the QEMU system emulator — never inferred, consistent with §6.2), a `profile`, and a primary `disk` size. Boot media is the user's problem by design — typically a `cdrom`/floppy attachment, often built from a folder (§6.3). `scratch` never appears in the store, cannot be pushed/pulled, and `template build` blocks may also use it as their source for building templates from pure installer media.

---

## 7. VM lifecycle

### 7.1 Clones

`vmlab up` creates each VM's disk as a qcow2 **linked clone** backed by the template image in the store (`scratch` VMs get a blank qcow2 instead, §6.5). Clones live in `.vmlab/` and are disposable: `destroy` deletes them; `down` powers off but keeps them. Templates are never written to by labs; deleting a template that backs existing clones must be refused (or require `--force` with a clear warning).

### 7.2 Power operations

`start`, graceful `stop` (guest-agent shutdown, falling back to ACPI, falling back to hard kill after a timeout), `force stop`, `restart`. Bring-up order respects `depends_on`: VMs with satisfied dependencies start in parallel; a dependency is satisfied when the VM is **ready** (agent responding) and any provision steps scoped to it have completed.

### 7.3 Snapshots

Both **online** and **offline** snapshots are required:

- **Offline** (VM powered off): disk state only.
- **Online** (VM running): disk + RAM + device state, taken without stopping the guest beyond the unavoidable pause.

Every snapshot records the VM's **power state at capture time**. Restore must do the right thing: restoring an online snapshot resumes the VM running exactly where it was; restoring an offline snapshot leaves the VM powered off. Snapshots are named, listable, and deletable per VM; a lab-wide snapshot verb captures all VMs (and containers, §18) in a lab under one name (consistency across VMs is best-effort, not coordinated — document this).

Snapshots use **qcow2-internal snapshots wherever the mechanism supports the case**, keeping the on-disk footprint to the clone file itself; external snapshot files are permitted only where internal snapshots cannot deliver the behavioural contract above. Either way the mechanism must coexist with the linked-clone backing chain, and the contract — not the mechanism — is what binds. Shared folders (§7.5) are SMB-based precisely so they carry no device state into snapshots.

### 7.4 Guest agent

The QEMU guest agent is the channel for: readiness detection, command execution with captured stdout/stderr/exit code, file copy in both directions, graceful shutdown, and IP address reporting. Templates are expected to install the agent during build; the windows profiles' build flow should make agent installation a documented, scriptable step. A VM without an agent still works for screen-driven automation but never reports **ready** — provision scripts targeting it must rely on screen/time waits.

### 7.5 Shared folders

A VM may declare shared folders mapping a host directory to a guest path:

```
vm "dev01" {
  ...
  share { host = "./src"      guest = "/mnt/src" }
  share { host = "~/datasets" guest = "D:\\data"  readonly = true }
}
```

**Mechanism: SMB, served by the daemon at the segment gateway.** Each declared share is exposed as `\\<gateway>\<share>` on the VM's segment. This was chosen over virtio-fs deliberately, after working through the alternatives:

- **No snapshot conflict.** virtio-fs (vhost-user-fs) carries FUSE session state outside QEMU, which historically made VMs unmigratable and blocks savevm-style online snapshots; the modern QEMU 8.2+ state-transfer path exists but is fragile for restore-much-later scenarios. SMB is pure network traffic — zero device state in the snapshot. A restored VM's SMB sessions are stale TCP that the guest's SMB client transparently re-establishes; mounts persist.
- **No guest driver burden.** Windows speaks SMB natively — nothing extra in template builds. Linux needs only `cifs-utils` (kernel CIFS is ubiquitous).
- **`windows-legacy` works** instead of being excluded — SMB2 covers Windows 7/2008R2-era guests, and **SMB1 (NT1/CIFS) is supported for guests that predate SMB2** (XP/2003-era): `smb1 = true` on a share enables the SMB1 dialect *and* the auth relaxation those guests require (NTLMv1/LM acceptance — XP doesn't send NTLMv2 by default). Off unless asked for; irrelevant as a security concern on an isolated lab segment, which is the whole reason vmlab can offer what the rest of the world has rightly abandoned.
- **No memory-backend constraint**, no per-share `virtiofsd` processes.
- Performance is worse than virtio-fs would have been — accepted under the §1.2 non-goal.

**Access model.** vmlab generates per-lab SMB credentials automatically; a share is mappable only with its owning VM's credential, scoping shares to their declaring VM even on a shared segment. Authenticated NTLMv2 + SMB signing is the baseline — required anyway because current Windows hardening (guest-auth blocking, signing requirements on recent Windows 11) rejects unauthenticated shares. None of this is user-visible: credentials are plumbed by vmlab.

**Guest mounting** is performed through the guest agent once the VM is ready:

- **Linux:** `mount -t cifs //<gateway>/<share> <guest_path>` with the generated credential.
- **Windows:** mapped via the SMB client with the generated credential. A drive-letter `guest` target maps directly; a folder-path target is realised as a directory symlink/junction to the UNC path. **⚠ Implementation note:** verify the folder-path mechanism (mklink-to-UNC behaviour, profile-vs-machine mapping persistence across reboots) against current Windows at implementation time.

**Server implementation.** No mature embeddable SMB *server* library exists in the Rust ecosystem (clients only, verified at time of writing), so this is the largest single engineering component the feature implies. Two permitted strategies behind the identical WCL/user surface:

1. **Embedded minimal SMB server in the daemon** — the design goal: SMB2 (negotiate/session(NTLMv2)/tree/create/read/write/query-directory/close + signing) plus the **SMB1/NT1 dialect for `smb1` shares** — a second, older protocol surface (different framing, NTLMv1/LM auth) that materially enlarges this component; no oplocks, no DFS. Self-contained, no dependencies.
2. **Bundled `smbd` as an interim backend** — the daemon generates config, runs Samba unprivileged on a localhost high port, and the switch proxies the segment gateway's port 445 to it. Samba still ships an SMB1 server behind explicit configuration (`server min protocol`), so this backend covers `smb1` shares from day one. Cost: a Samba dependency (bundled in the container image; documented host package otherwise) — **⚠ verify at implementation** that the bundled/target Samba build retains NT1 support, since distros increasingly trim it.

The PRD permits shipping 2 first and replacing with 1 later — including a hybrid where the embedded server handles SMB2 shares and `smb1` shares route to smbd; the user surface must not change between strategies.

**XP-era caveat, stated honestly:** the QEMU guest agent is unlikely to be available for XP/2003 guests (modern virtio-win and qemu-ga builds dropped that era), so vmlab's automatic agent-driven mounting won't apply. For those guests the mount is performed by the provision script through the screen-automation surface (§10.3 keystrokes — `net use X: \\<gateway>\<share> /user:...`), which is exactly the kind of guest those APIs exist for. The docs should include this as a worked example.

**Constraints, stated plainly:**

- Share *contents* are host state, outside snapshot scope — restore never rolls back files. The docs must say this loudly.
- A VM's shares are reachable only via a segment its NIC sits on; a VM with no NICs cannot have shares (validation error) — consistent with air-gapped-by-default.
- Port-isolated NICs (§9.1) can still reach the gateway, so shares work on isolated ports by design.

---

## 8. Events, handlers, logging

### 8.1 Events

The daemon emits structured events, minimally:

- **Lifecycle:** `vm.starting`, `vm.ready`, `vm.stopped` (with reason: requested / guest-initiated / crashed), `vm.crashed`, `lab.up`, `lab.down`, `snapshot.created`, `snapshot.restored`, `template.built`
- **Errors:** QMP failures, QEMU process death, agent timeouts, network fabric errors, `lab.daemon_crashed` (emitted by the supervisor) — any unrecoverable error is an event before it is a failure.
- **Resource watchdog:** `host.disk_low` (configurable threshold on the filesystems holding `.vmlab/` and the template store — linked clones grow), plus headroom checks before snapshot operations.

### 8.2 Handlers

`on "<event>" run "<script.ws>"` in the lab file binds events to wscript handler scripts, which receive the event payload and a lab handle. Handler failures are logged, never fatal to the daemon. Typical uses: collect artefacts on crash, alert on disk pressure, restart policies implemented in script rather than baked into the daemon.

Lab daemons emit their own events and forward them to the supervisor, which maintains the host-wide aggregate stream; subscribers can attach at either level.

### 8.3 Logging

Everything is logged: daemon log, per-lab log, per-VM log (QEMU stdout/stderr, QMP traffic at debug level, agent operations, network rule changes), all as **JSON lines** under `~/.local/state/vmlab/`. `vmlab logs [lab/]vm` tails or dumps; provision script output is captured into the lab log and streamed live to the invoking CLI.

---

## 9. Networking

The daemon contains a complete userspace network stack. This is vmlab's defining feature and the section with the most novel implementation surface.

### 9.1 The switch

Each segment is a virtual L2 switch inside its owning daemon (lab daemon for lab-scoped segments, supervisor for global ones — §3). Every VM NIC connects via a QEMU **stream-socket netdev** over a unix socket in `$XDG_RUNTIME_DIR/vmlab/`; the owning daemon does MAC-learning frame forwarding between ports of the same segment. Consequences, all deliberate:

- **No privileges required.** No tap devices, no bridges, no macvlan, no CAP_NET_ADMIN — which is precisely what makes WSL2 a first-class host.
- The daemon sees every frame, which is what makes DHCP, DNS, routing, filtering, and redirection (below) implementable as switch participants rather than external services.
- **Port isolation:** any NIC may set `isolated = true`. The switch then drops guest-to-guest frames for that port (the private-VLAN model) — the NIC can reach the daemon's gateway services (DHCP/DNS), the segment's NAT port, port-forwards, and daemon routing, but never neighbouring guests. Works on any segment, built-in or declared.
- Throughput is a stated non-goal (§1.2). The netdev attachment is designed so a vhost-user or tap backend can be substituted per segment later without changing lab semantics.

### 9.2 Segments and namespacing

- Segments are **lab-scoped by default**: `corp` in lab A and `corp` in lab B are different wires.
- A segment declared `global = true` (or addressed by a namespaced name, e.g. `shared/backbone` — exact scheme per WCL conventions) is owned by the **supervisor**, created on first attach, destroyed on last detach, and shared by every lab that attaches to that name. Lab daemons attach via segment trunks (§3.1); the supervisor runs the shared segment's DHCP/DNS so registrations span labs coherently. Cross-lab VMs on a shared segment get mutual DNS registration (§9.6).
- **Cross-host attach:** a segment may declare a remote peer (`connect { host = "helios:port" }`); the two **supervisors** bridge the segment by tunnelling L2 frames over the same trunk protocol used locally (§3.1), over TCP. This is the entire cross-host story for v1 — VMs stay local, wires can span hosts. Supervisor-to-supervisor links authenticate with a **pre-shared key** configured on both hosts — deliberately simple; no certificate machinery in v1.

### 9.3 Multi-lab concurrency

Multiple labs run simultaneously. VM names are scoped per lab; the CLI addresses `vmlab <cmd> <vm>` using the cwd's lab context, or `lab/vm` explicitly from anywhere.

### 9.4 DHCP

**On by default for every segment.**

- Subnets auto-allocate as /24s carved from a host-wide pool — default **10.213.0.0/16**, overridable in host-level daemon config — when not declared. Declared subnets are honoured.
- The daemon serves leases at the segment gateway. VM NICs with a static `ip` become **DHCP reservations** keyed on the NIC's persisted MAC — guests keep plain DHCP config and still land on deterministic addresses. Static IPs may sit outside the dynamic pool.
- DHCP options served: gateway, DNS server (the daemon by default — overridable per segment to e.g. a DC, or suppressible), domain suffix, and **classless static routes (option 121)** from the segment's `routes {}` declarations.
- Per-segment opt-out: `dhcp = false` for segments where a lab VM (DC, pfSense, dnsmasq experiment) should own addressing.

### 9.5 DNS

**On by default**, answering on each segment's gateway address.

- **Auto-registration:** every guest NIC registers as `<vm>.<lab>.<suffix>` (and a short `<vm>.<suffix>` alias where unambiguous within the segment). Suffix configurable; default **`vmlab.internal`** (avoiding `.local`/mDNS collisions).
- **Static entries:** arbitrary name→IP records declared per segment or per lab, wildcards supported.
- **Forwarding:** unresolved queries go to a configurable upstream, defaulting to the host's resolver — guests on NAT'd segments get working public DNS for free.
- Global segments resolve names across all attached labs.
- Per-segment override of the DNS server handed out via DHCP, or full opt-out — AD labs need the DC to own DNS.

### 9.6 Routing

Two distinct mechanisms, both in v1:

1. **Guest routes via DHCP option 121** — segment-declared `routes {}` are pushed to every guest at lease time. The mechanism for multi-segment topologies routed through a **VM** (firewall/router labs).
2. **Daemon inter-segment routing** — the daemon itself forwards L3 between two segments. **Explicit opt-in per segment pair, never default**; segments are isolated unless connected by declaration or by a router VM. Declared in WCL and toggleable at runtime from scripts.

### 9.7 NAT / internet egress

Internet egress is provided by a **slirp/passt-style userspace NAT attached as a port on a switch** — outbound TCP/UDP/ICMP from guests translated to host sockets, no privileges required. Two ways to get it:

- **Declared segments:** `nat = true` on the segment. Off by default — declared segments are isolated unless you say otherwise.
- **The built-in `nat` segment:** `nic { nat = true }` attaches the NIC to a per-lab, daemon-provided NAT segment — DHCP, DNS, and egress on, nothing to declare. It is a shared segment within the lab, so VMs using the shorthand can reach each other by default. Adding `isolated = true` to the nic (port isolation, §9.1) keeps the zero-declaration egress while cutting the NIC off from neighbouring guests.

Combined with the no-NIC default (§5.2), the connectivity ladder is: nothing → `nat = true` shorthand → declared segments.

### 9.8 Port forwarding

Declared in WCL (`forward { host_port = 13389 to = "dc01:3389" }`) or created at runtime via CLI/script. The daemon listens on the host address and proxies TCP and UDP into the segment. This is the host→guest access path (RDP, SSH, web UIs) and works identically under WSL2 (where Windows-side access then rides WSL's own localhost forwarding).

### 9.9 Filtering and redirection

Two enforcement layers, declared in WCL and **mutable at runtime from wscript scripts** — runtime mutation is a first-class lab scenario ("block the DC, watch the client fail over"). Static filtering and redirection belong in `vmlab.wcl`; there is no `vmlab net` CLI surface:

- **DNS rules** (per segment or lab): sinkhole a name (NXDOMAIN or 0.0.0.0) or override it to a chosen IP. Wildcards supported (`*.telemetry.example.com`). Only effective for guests using the segment DNS.
- **L3 rules at the switch** (per segment): match on IP/CIDR and optionally protocol/port.
  - **block** — drop, answering with ICMP unreachable / TCP RST where feasible so guests fail fast.
  - **redirect** — DNAT: traffic to X[:port] rewritten to Y[:port], with the daemon maintaining the connection state to rewrite return traffic.

Evaluation order: redirect rules before block rules; within a layer, most-specific match wins; ties broken by declaration order. The full resolution algorithm must be specified precisely in the implementation design doc.

---

## 10. wscript scripting surface

> **⚠ Binding note.** Function names, signatures, module/import syntax, and the shape of `Value`/`Result` types below are illustrative. The real surface binds to the wscript spec; vmlab registers its API as a wscript host module and ships the corresponding `.wscripti` interface file so script authors get full LSP support (diagnostics, hover, completion), mirroring the Config Weave approach.

Scripts are **daemon-unaware**: they receive a lab handle and operate on it. Provision scripts and event handlers use the same API.

### 10.1 Lab handle

```
lab.vm("dc01") -> Vm                 # error if undefined in the lab
lab.vms() -> [Vm]
lab.segment("corp") -> Segment
lab.name() -> string
lab.log(msg)                         # into the lab log + live CLI stream
```

### 10.2 Segment handle

```
seg.block(cidr, opts)    seg.unblock(rule_id)
seg.redirect(from, to, opts)
seg.dns_set(name, ip)    seg.dns_sinkhole(pattern)   seg.dns_clear(...)
seg.route_to(other_segment)          # opt-in inter-segment routing, reversible
seg.forward(host_port, vm, guest_port) -> rule_id
seg.rules() -> [Rule]
```

### 10.3 VM handle

**Lifecycle / state**

```
vm.start()  vm.stop()  vm.stop_force()  vm.restart()
vm.state() -> Running | Stopped | ...
vm.wait_ready(timeout)               # agent responding
vm.wait_shutdown(timeout)
vm.ip(nic?) -> string                # from lease table / agent
```

**Snapshots**

```
vm.snapshot(name)                    # online or offline per current state
vm.restore(name)                     # resumes running iff snapshot was online
vm.snapshots() -> [SnapshotInfo]     # name, taken_at, power_state
vm.delete_snapshot(name)
```

**Input**

```
vm.send_keys("ctrl-alt-del")         # chords, QMP sendkey naming
vm.type_text("Password1!\n", opts)   # human-ish pacing options
vm.mouse_move(x, y)  vm.mouse_click(button)  vm.mouse_drag(...)
```

**Screen**

```
vm.screenshot(path?) -> Image
vm.wait_for_image(ref, opts) -> Match        # opts: timeout, threshold,
vm.wait_for_any([refs], opts) -> Match       #   region{x,y,w,h}, interval
vm.find_image(ref, opts) -> Match | None     # single-shot, no wait
vm.ocr(opts) -> string                       # Tesseract; optional region
vm.wait_for_text(pattern, opts) -> Match     # OCR-based wait, regex pattern
```

Reference images are paths relative to the lab (convention: `images/` beside `vmlab.wcl`). Matching is normalised template matching with a similarity threshold (default ~0.9, overridable). `Match` carries location + score, so a found image can anchor a relative mouse click.

**Guest agent**

```
vm.exec(cmd, opts) -> ExecResult     # exit_code, stdout, stderr; timeout opt
vm.copy_to(local, guest_path)
vm.copy_from(guest_path, local)
```

All blocking calls take timeouts and return wscript `Result`s; an error propagating out of a provision script fails the provision run (and therefore `vmlab up`) with the error attached to the lab log.

### 10.4 Execution model

- Provision scripts listed in `vmlab.wcl` run in declaration order during `up`, after the VMs they reference are started per `depends_on`. A script orchestrating multiple VMs (stand up DC → wait → join member) is the expected normal case.
- Any script is also invocable ad hoc: `vmlab script scripts/whatever.ws`.
- Event handlers receive `(event: Value, lab)` — the one dynamic escape hatch, consistent with Config Weave's boundary model.
- Template build scripts get the same API scoped to the single build VM (a lab handle containing one VM).

---

## 11. Console access

Every VM gets a VNC display served on a unix socket (TCP optional, off by default). `vmlab console [lab/]vm` connects — launching a configured viewer, with a TCP-forward fallback for environments (WSL2) where the viewer lives on the Windows side. SPICE is explicitly deferred. VMs are headless by default in the sense that nothing attaches unless asked; the display always exists so screenshots and console attach work at any moment.

`gui = true` (per VM, or as a lab-wide default) is a convenience that auto-opens a viewer on `vmlab up` — and watches the build VM during `vmlab template build`. It is **never** QEMU's own GTK window: the VM always runs headless behind VNC, so the viewer is a separate client process and closing its window only disconnects (the VM keeps running; `vmlab console` reattaches). The viewer is launched from the interactive CLI, not the headless lab daemon.

The viewer is chosen automatically: an explicit `viewer` in host config wins, else the first of `remote-viewer` (virt-viewer), `gvncviewer`, `vncviewer` found on `PATH`. `remote-viewer` dials the VNC unix socket directly; the others are TCP-only, so vmlab bridges the socket to a localhost display port for them. Neither `vmlab console` nor the `gui = true` auto-open ties up the terminal: a TCP viewer's bridge runs in a detached helper (`vmlab __vncbridge`, in its own session) that exits when the viewer window closes. With no viewer at all, `vmlab console` falls back to bridging and printing the address to attach to manually (the WSL2 path, also forced with `--tcp`).

---

## 12. CLI

| Verb | Action |
|---|---|
| `vmlab up [vm...]` | Create/start lab (or subset), run provision scripts |
| `vmlab down [vm...]` | Graceful stop; clones retained |
| `vmlab destroy` | Stop + delete clones, lab-local state, dynamic net config |
| `vmlab status` | Lab/VM/segment state, IPs, ready flags |
| `vmlab validate` | Full §5.1 validation, no side effects |
| `vmlab vm start / stop / restart <vm>` | Per-VM power control |
| `vmlab snapshot create / restore / list / delete` | Per-VM or lab-wide (§7.3) |
| `vmlab console <vm>` | Attach viewer |
| `vmlab exec [--timeout s] <vm> -- cmd` | Guest-agent exec |
| `vmlab cp <src> <vm>:<dest>` | Copy a host file/tree into a guest via the agent |
| `vmlab osinfo <vm>` | Guest OS identification as JSON |
| `vmlab script <script.ws>` | Ad-hoc script against the current lab |
| `vmlab logs [lab/][vm]` | Tail/dump JSON-line logs |
| `vmlab template build / list / rm / export / import` | Template store |
| `vmlab template push / pull / login` | OCI registry distribution (§6.4) |
| `vmlab daemon start / stop / status` | Supervisor control (normally automatic); status lists lab daemons |

---

## 13. WSL2 considerations (summary)

Everything above was chosen to be WSL2-clean, but to state it once: KVM requires nested virtualisation enabled in `.wslconfig`; networking uses no tap/bridge/macvlan so no WSL kernel or privilege gymnastics; host access from Windows rides port-forwards + WSL localhost forwarding; `$XDG_RUNTIME_DIR` must be verified/created at daemon start (some WSL setups lack it); and the disk-space watchdog matters more here because the ext4 VHDX grows.

---

## 14. Official container image

vmlab ships an official Docker/OCI **runtime image** (distinct from template artifacts, §6.4) containing the vmlab binary plus its full runtime dependency set: QEMU (system emulators for supported arches), OVMF/SeaBIOS firmware, swtpm, Tesseract, passt/slirp, and a VNC-capable toolchain. Published per release alongside the binary (e.g. `ghcr.io/<owner>/vmlab:<version>`).

- **Acceleration.** With `--device /dev/kvm` the container runs KVM-accelerated like a native install. Without it, vmlab falls back to TCG with a loud warning — slow but functional, which matters for environments without KVM exposure.
- **Privileges.** Because the network fabric is userspace, the container needs **no** `--privileged`, no extra capabilities, and no host network mode — `/dev/kvm` is the only host grant.
- **State.** Documented volume mounts for the template store (`~/.local/share/vmlab/templates`) and the lab directory; everything else is container-ephemeral by design. Host access to guests rides vmlab port-forwards mapped out with ordinary `-p` flags.
- **Entrypoint.** Defaults to the supervisor in the foreground (lab daemons are its children); `docker exec` (or a second container sharing the socket volume) drives the CLI. A one-shot mode (`vmlab up && vmlab script ...` then exit) suits CI.
- **Primary use cases:** CI pipelines running full lab tests, trying vmlab without installing QEMU, and pinning a known-good QEMU version independent of the host distro.

---

## 15. Suggested milestones

1. **M1 — Core lifecycle:** supervisor + lab daemon split with socket protocol, WCL schema + validate, template store (import existing qcow2 only), linked clones, start/stop, QMP, guest agent exec/copy, single NAT'd zero-config segment, logs.
2. **M2 — Automation surface:** wscript host module (lifecycle, exec, keys, screenshot, image match, waits), provision scripts, `run`, snapshots both modes.
3. **M3 — Network fabric:** named segments, DHCP + reservations + option 121, DNS + registration + forwarding, port forwards, console/VNC.
4. **M4 — Template builds + shares:** ISO sources w/ URL+hash, media building, build scripts, export/import, profiles complete incl. legacy, SMB shared folders (smbd backend acceptable initially per §7.5).
5. **M5 — Advanced networking + events:** global segments, cross-host attach, inter-segment routing, filtering/redirection + runtime mutation, event handlers, watchdogs, OCR.
6. **M6 — Distribution:** OCI push/pull with chunking and multi-arch indexes, registry auth, lab references to registry templates, official container image.

## 16. Resolved decisions

Formerly open; all resolved 2026-06-12 and folded into the sections referenced:

| # | Decision | Resolution |
|---|---|---|
| 1 | Default DNS suffix | `vmlab.internal` (§9.5) |
| 2 | Auto-subnet pool | /24s from 10.213.0.0/16, overridable in host config (§9.4) |
| 3 | NAT defaults | No NICs = no network; declared segments NAT off; `nic { nat = true }` shorthand → per-lab built-in NAT segment (§9.7) |
| 4 | Snapshot mechanism | qcow2-internal wherever possible — keeps disk clean; external only where internal can't meet the contract (§7.3) |
| 5 | Cross-host auth | Pre-shared key, kept deliberately simple (§9.2) |
| 6 | OCR binding | Implementation detail; §10.3 API binds |
| 7 | wscript runtime location | Inside the lab daemon — co-located with events and state (§3) |
| 8 | OCI chunk default | 512 MiB zstd; sized against GHCR's 10 GB/layer limit and 10-minute upload timeout (§6.4) |
| 9 | OCI media/artifact types | `application/vnd.vmlab.*.v1` family; freeze before first public push |
| 10 | Lab-daemon crash handling | Supervisor marks failed + emits event; no auto-restart — restart policy belongs to script handlers (§3, §8) |

---|---|---|
| 1 | Default DNS suffix | `lab.internal` (avoid `.local`) |
| 2 | Auto-subnet pool default range/size | A /16 from RFC1918 carved into /24s, configurable; pick one unlikely to collide (e.g. within 10.213.0.0/16) |
| 3 | ~~NAT defaults~~ | **Resolved:** no NICs = no network; declared segments NAT off; `nic { nat = true }` shorthand attaches to per-lab built-in NAT segment (§9.7) |
| 4 | Snapshot mechanism (internal vs external) | Implementation design doc; behaviour contract in §7.3 binds |
| 5 | Daemon-to-daemon auth for cross-host segments | PSK minimum; design doc |
| 6 | OCR engine binding (Tesseract via library vs subprocess) | Implementation detail; API in §10.3 binds |
| 7 | Where the wscript runtime executes | Implementation detail; daemon-unaware API binds |
| 8 | Default OCI chunk size + compression level | ~512 MiB zstd; verify against current registry per-blob limits at implementation |
| 9 | Exact vmlab OCI media/artifact type strings | `application/vnd.vmlab.*.v1` family; freeze before first public push |
| 10 | Supervisor behaviour on lab-daemon crash | Mark failed + event only (no auto-restart); revisit if restart policies prove wanted beyond script handlers |

---

## 17. Out-of-scope ideas recorded for later

vhost-user / tap fast paths per segment; SPICE; a TUI; daemon inter-segment routing policies beyond pair allow; PCAP capture per segment (the switch sees everything — cheap and very lab-useful, first candidate for v1.1); record/replay of input scripts; per-lab resource limits (the per-lab daemon makes a cgroup subtree per lab a natural extension); replacing the interim smbd share backend with the embedded SMB2 server (§7.5) if v1 ships with smbd.

---

## 18. Lab containers (micro-VM)

Labs may declare **OCI containers** alongside VMs. A `container` block names a
standard container image (`nginx:1.27`, `ghcr.io/owner/app@sha256:…` — Docker
Hub shorthand normalises to `registry-1.docker.io`) plus compose-style
configuration: `env {}` variables, `volume {}` binds and named volumes,
`entrypoint`/`command`/`workdir`/`user` overrides, `port {}` host forwards, a
`healthcheck {}`, a `restart` policy, and `nic {}` blocks identical to VM NICs.
VM and container names share one namespace: DNS, `depends_on` waves,
`forward { to = "name:port" }` targets, and provision scoping all resolve
across both kinds. A container with no NICs is valid — air-gapped, still
reachable via `exec`/`cp` over the agent channel — unless it declares
volumes, which are network-mounted (validation error, mirroring §7.5).

**Architecture.** Every container runs in a **micro-VM**: a pinned Alpine
`linux-virt` kernel + purpose-built initramfs (`vmlab-cinit` as PID 1,
bundling `qemu-ga`), booted directly with `-kernel/-initrd`, 1 vCPU / 256 MiB
by default. The image's layers are flattened (whiteout-aware, tar-level, no
host privileges) into a squashfs mounted read-only, with a per-container
scratch qcow2 as the overlayfs writable layer. Config reaches the guest as a
`ContainerSpec` pushed over the ctl channel (the virtio-serial port that also
carries lifecycle events and stop/resync commands). Volumes are SMB shares
served by the lab daemon at the segment gateway — the same bundled-`smbd`
mechanism as §7.5 shared folders — mounted by cinit over CIFS once the
network is up. No virtfs/9p device is attached, which is what keeps the
micro-VM fully snapshottable (§7.3): SMB is pure network traffic and carries
no device state into a snapshot. Ownership on volume files is mount-level
(CIFS), not per-file container uid/gid. This preserves the §14 guarantee verbatim:
containers work inside the official image with **no** `--privileged`, no added
capabilities — `/dev/kvm` only, TCG fallback without it.

**Networking.** A container NIC is a VM NIC: the same `-netdev stream` unix
socket into the segment switch, the same DHCP lease/reservation, DNS
registration (`<name>.<lab>.<suffix>`), NAT egress, and L3 rules. `port {}`
blocks are sugar for the segment forward machinery, installed against the
container's lease when it turns ready and re-installed after restarts.

**Lifecycle.** Image references resolve like registry templates (§6.4): the
digest resolved at first pull is pinned in lab state and never re-pulled
implicitly; `vmlab container destroy` (or editing the `image =` line) clears
the pin. The supervisor pre-pulls images with `container.pull.*` progress
events before spawning the lab daemon. Readiness is two-stage — process
started, then the first passing healthcheck (when declared) — and gates
`depends_on` waves. Exit handling is host-side: `restart = "on-failure" |
"always"` respawns with 1→30 s backoff (giving up after 5 rapid failures);
events `container.starting/ready/stopped/crashed/unhealthy` are bindable with
`on {}`. The stop ladder mirrors VMs: in-guest stop signal + grace, then
guest shutdown, then kill. Container stdout/stderr is the serial console log
(`vmlab container logs [-f]`).

**Snapshots.** Containers are snapshottable with full VM parity (§7.3).
Offline snapshots capture the scratch disk; online snapshots capture scratch
+ RAM + device state, both as qcow2-internal snapshots on the per-container
scratch qcow2 (the read-only rootfs squashfs is immutable and outside the
snapshot). The power state recorded at capture drives restore exactly as for
VMs — an online snapshot resumes the container mid-flight, process state and
all — and lab-wide snapshot verbs cover containers alongside VMs. Volume
contents are host state outside snapshot scope, exactly like §7.5 shares —
restore never rolls back volume files, and like a restored VM's SMB session
the volume's CIFS session is stale TCP the client re-establishes (the
micro-VM mounts with `echo_interval=5`, so the first post-restore volume
access stalls seconds, not minutes). Each snapshot records the container's
pinned image digest; restoring under a different pin fails (the writable
layer is only valid against the same rootfs).

**Deliberate exclusions.** Named volumes are lab-scoped, shared
by name, and survive `down` and per-container destroy; only lab `destroy`
removes them. Cross-arch containers (image arch ≠ host arch) are out of scope
for v1, as is a crun/native runtime backend (recorded as a possible fast path
if micro-VM start latency ever matters).
