# vmlab — Product Requirements Document

**Status:** Draft v1
**Date:** 2026-06-12
**Depends on:** WCL (spec complete, implemented), wscript (assumed complete before this PRD is executed)

---

## 1. Overview

vmlab is a single-host virtual machine lab orchestrator. It defines **labs** — named groups of VMs and virtual networks — declaratively in WCL, builds and manages reusable **templates** (the Vagrant-box analogue), and drives lab automation through **wscript scripts** that can interact with guests at every level: power state, snapshots, keystrokes and mouse input, screenshot capture with image matching and OCR, and command execution and file transfer via the vmlab guest agent (`vmlab-agent`, a virtio-serial channel).

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
| **Ready** | A VM is *ready* when its vmlab guest agent answers its handshake. A lab is *up* when all VMs are ready and all provision scripts have completed. |
| **Dev machine** | A machine marked `@dev`, published as an SSH endpoint an editor attaches *into* (§19). VM or container, Windows or Linux — one contract for both. |
| **Workspace** | A dev machine's source tree: a guest-local working copy of a canonical host directory, kept in step by vmlab's syncer (§19.6). Not a `share {}`. |
| **Login** | A labelled identity declared on a machine — an account, its secret, and whether it is elevated. What a surface attaches *as* (§19.2). |
| **SSH facade** | The SSH server vmlab terminates on the host, servicing channels through the guest agent. No sshd runs in any guest (§19.3). |

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

- **Supervisor (`vmlabd`).** One per user, auto-started by the CLI. Owns: spawning a lab daemon on `up` and reaping it on `down`/`destroy`; the registry of running labs (name → root, socket, pid, state); **global segments** (§9.2) — shared switches live here, with lab daemons attached as trunk ports; cross-host peering; serialised writes to the template store (pulls, builds, imports — so concurrent labs can't corrupt it; reads are lock-free); host-level watchdogs (`host.disk_low`); and an aggregated event stream across all labs. A lab name is unique within this host registry. `lab.ensure` and `lab.restart` refuse a name already registered at another canonical root with a `conflict` error naming that root and telling the caller to stop the other lab or rename this one. If a lab daemon dies unexpectedly, the supervisor detects it, emits `lab.daemon_crashed`, and marks the lab failed — it does not silently restart it.
- **Lab daemon.** One per running lab, owning everything lab-scoped: that lab's QEMU processes, QMP and guest-agent channels, lab-local segments and their DHCP/DNS/routing/rules, clones and snapshots, lab state, and lab events (forwarded up to the supervisor's aggregate stream). A lab daemon's failure is contained to its lab; other labs and the supervisor are unaffected.
- **CLI.** Connects to the supervisor for discovery and host-scoped verbs (template store, `status` across labs, daemon control), then talks **directly** to the relevant lab daemon's socket for lab-scoped operations — no proxying in the hot path.
- **wscript runtime.** Executes **inside the lab daemon** — it must react to events and co-locating it with the lab's state and event stream keeps everything in one place. The script-facing contract is unchanged: scripts get the lab/VM API (§10) and remain daemon-unaware.
- **QEMU.** One process per VM, launched by its lab daemon with `-qmp`, the `vmlab.agent.0` virtio-serial channel, VNC display, and one stream-socket netdev per NIC into the lab daemon's switch.

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

    # Configuration steps live in the machine they configure, and run in the
    # order they appear here once it is ready.
    provision "scripts/setup.ws" { }
    playbook "playbooks/baseline" {
      play = "baseline"
      var "tz" { value = "UTC" }       # --var tz=UTC, for this machine only
    }
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

  on "vm.crashed"    run "scripts/collect-dumps.ws"
  on "host.disk_low" run "scripts/alert.ws"
}
```

### 5.1 Validation

Configuration steps target structurally: `provision {}` and `playbook {}` are declared *inside* the `vm {}` or `container {}` they configure, so the machine is the target and there is nothing to cross-reference. A machine's steps run in the order its blocks appear, once it is ready; across machines they follow the order the machine blocks appear, with `depends_on` gating when each becomes eligible. A `playbook {}` may carry `var "<name>" { value = "…" }` children, passed to config-weave as `--var name=value` for that machine's run only — the mechanism for giving one play different settings per machine.

`vmlab validate` (and implicitly every other verb) evaluates the lab file against the vmlab WCL schema and fails before any side effect on errors including: unknown attributes, missing templates, undeclared segment references, static IPs outside their segment's subnet, duplicate static IPs/MACs, dependency cycles in `depends_on`, missing script files, archless or malformed template references, `scratch` VMs missing `arch`/`profile`/`disk`, `secure_boot = true` on a VM whose firmware resolves to SeaBIOS (§5.2 — either value may have been inherited, so the message names the layer each came from), playbook variable names that are not WCL identifiers or are set twice on one block, and wscript compilation errors in all referenced scripts. The goal mirrors Config Weave: validation catches everything that can be caught without touching QEMU.

Dev machines (§19) add four rules, all cross-block — the ones WCL's own
decorator and schema validation structurally cannot see:

- More than one `@dev(default = true)` in a lab, naming both machines (§19.1).
- `login { user = … }` with no `password` on a **Windows-family** profile (§19.2).
- `elevated` on a **Linux-family** profile (§19.2).
- More than one `login` with `default = true` on a machine, naming both (§19.2).

`@dev` on a machine whose agent cannot serve an attach is deliberately **not** a
validation error; §19.4 says where it fails instead and why.

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
| `windows-legacy` | i440fx or q35 | SeaBIOS | none | IDE/SATA disk, e1000 NIC, std VGA — for Vista/7/2008-era guests with no virtio storage/net drivers at install time; virtio-serial agent channel (virtio-win covers this era) |
| `windows-xp` | i440fx | SeaBIOS | none | as `windows-legacy`, but `agent_transport = "isa-serial"`: NT4 through XP/2003 have no virtio drivers at all, so the legacy agent (§7.4) speaks over a 16550 on COM1 |
| `windows-9x` | i440fx | SeaBIOS | none | IDE disk, PCnet NIC, Cirrus VGA, VNC input, `isa-serial` agent channel — DOS, Windows 3.x through ME |
| `templeos` | i440fx | SeaBIOS | none | IDE disk, std VGA (it drives VBE directly), no network by design; `agent_transport = "isa-serial"` for the HolyC agent |
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
- **Provision scripts** — the same wscript machinery as labs (§10): the build boots the source, the script drives the installer with keystrokes/screen matching, configures, and seals. The vmlab guest agent is installed by the guest's own unattended-install hook from the auto-attached VMLAB bootstrap ISO, and the build verifies its handshake before sealing.

Build flow: create working qcow2 → boot per template hardware → run build provision scripts → graceful shutdown → move qcow2 + metadata into the store under `<arch>/<name>/<version>/`. Metadata records both the baked `agent_version` and the host wscript-surface version used by embedded first-boot scripts. A failed build leaves nothing in the store.

### 6.2 Store, addressing, export

- Store key is **arch + name + version**. References take the form **`<arch>/<name>[@<version>]`** — arch is mandatory, always explicit, never inferred from the host; version omitted means highest in the store. `vmlab validate` rejects archless references.
- Each store entry's `template.wcl` records its hardware and provenance, baked agent version, and the wscript-surface version used by embedded scripts. A missing surface version identifies a legacy pre-versioning template and is accepted.
- `vmlab template list / rm` manage the store.
- `vmlab template export` produces a single portable archive (qcow2 + metadata); `vmlab template import` installs one — the offline/sneakernet sharing path.
- The online sharing path is OCI registries (§6.4).

### 6.3 Media building

`vmlab` can build **ISO and floppy images from folders on disk**, declared inline in template/VM blocks (`media { type = "iso" from = "./unattend/" }`). Built images land in `.vmlab/` and are content-addressed so unchanged folders don't rebuild. Primary use: unattend/answer files, driver bundles, agent installers, payload delivery to guests with no network.

### 6.4 OCI registry distribution

Templates are distributable through standard OCI registries (GHCR, Docker Hub, Harbor, a self-hosted registry on Hermes — anything speaking the OCI distribution API), as **OCI artifacts, not container images**:

- **Artifact identity.** The manifest carries a vmlab-specific `artifactType` (e.g. `application/vnd.vmlab.template.v1`), and all blobs use vmlab media types. A `docker pull`/`docker run` against a vmlab reference must fail fast as "not a container image" rather than half-work — that's the whole point of typing it. Conversely, `vmlab template pull` refuses manifests that aren't vmlab artifacts.
- **Layout.** Config blob = template metadata (the recorded hardware, profile, agent version and wscript-surface version from `template.wcl`). Layers = the qcow2, **chunked**.
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

Snapshots use **qcow2-internal snapshots wherever the mechanism supports the case**, keeping the on-disk footprint to the clone file itself; external snapshot files are permitted only where internal snapshots cannot deliver the behavioural contract above. Either way the mechanism must coexist with the linked-clone backing chain, and the contract — not the mechanism — is what binds. Shared folders (§7.5) stay snapshot-compatible on both transports: SMB carries no device state at all, and virtiofs shares transfer the virtiofsd session state through the snapshot's migration stream.

### 7.4 Guest agent

The vmlab guest agent (`vmlab-agent`, one multiplexed virtio-serial channel — `vmlab.agent.0`) is the channel for: readiness detection, interactive terminals, streaming command execution with captured stdout/stderr/exit code, digest-verified file transfer in both directions, log tailing, metrics, clipboard, structured OS info, per-NIC IP address reporting, and graceful shutdown/reboot. Template builds stage the agent binaries plus an install script on an auto-attached **VMLAB bootstrap ISO**; the template's unattended-install hook (cloud-init runcmd, installer late-commands, autounattend first-logon) runs the script, and the build verifies the agent's handshake before sealing. A VM without an agent still works for screen-driven automation but never reports **ready** — provision scripts targeting it must rely on screen/time waits.

**The legacy tier.** A profile names the agent channel's device with `agent_transport`: `"virtio-serial"` (the default, above), `"isa-serial"`, or `"none"` (no agent is possible; the guest is screen-driven and never reports ready). With `isa-serial` the same host socket is wired to a 16550 UART on COM1, the serial console log moves to COM2, and the guest runs **`vmlab-agent-legacy`** (`guest/agent-legacy`): the same wire protocol, in C89 because Rust has no supported target for NT4 through XP/2003, Windows 9x/ME or DOS. It advertises one feature, `exec`, and refuses every other open by name on the channel that asked, so the §19.4 ladder degrades truthfully — readiness, `vmlab exec`, `os_info` and the stop ladder work; a terminal, `vmlab cp`, `dev attach` and `repair-agent` say what is missing. A `logon` on an exec is refused: nothing in that tier mints one. Two limits are deliberate: QEMU times UART transmit to the baud rate (about 11 KB/s at 115200), which is why no file transfer is offered; and DOS runs one program at a time, so on DOS the agent is the foreground program, output arrives after the command exits, stdin is acknowledged and discarded, and the agent answers nothing else while a command runs. The bootstrap ISO carries the three legacy builds (`legacy/nt`, `legacy/9x`, `legacy/dos`, 8.3-safe because DOS reads no Joliet) with an install script each; `install.cmd` defers to the NT one on a 4.x/5.x kernel. A Linux guest too old for virtio-serial needs no C agent: `vmlab-agent` takes COM1 itself when the VM has no virtio-serial controller, with its full feature set at serial speed. **TempleOS** joins the tier in its own language (`guest/agent-templeos`): HolyC, compiled by the guest, one task polling the same UART. A command there *is* HolyC source, which `ExePrint` compiles and runs. It cannot ride the bootstrap ISO, because TempleOS reads no ISO 9660 and has no network, so the one way in is the screen: `vmlab::templeos_agent_script()` returns the source as statements to type, and the provision that types them registers the agent in `~/MakeHome.HC` for every later boot. **⚠ Implementation note:** the transport, the handshake and the feature ladder are verified live; **capturing a command's output is not finished**. TempleOS has no output redirection hook — printing goes into the task's own document — so output must be read back from that document, which works in a task that has a window and yields nothing in the agent's spawned task. Until it does, a TempleOS guest answers `ready` and returns empty output, so the shipped template keeps `agent = false`.

§19 adds three capabilities to the agent, advertised as feature strings —
`tunnel`, `fileops` and `watch` (§19.4) — and **retires the whole-file,
path-addressed transfer** above onto `fileops`, keeping its guest-computed
digest verification (§19.5). The protocol version is unchanged.

### 7.5 Shared folders

A VM may declare shared folders mapping a host directory to a guest path:

```
vm "dev01" {
  ...
  share { host = "./src"      guest = "/mnt/src" }
  share { host = "~/datasets" guest = "D:\\data"  readonly = true }
}
```

**Mechanism: two transports behind one `share {}` surface.** Each share carries `transport = "auto" | "virtiofs" | "smb"` (default `auto`).

**virtiofs** is the fast path: one `virtiofsd` per share, attached as a vhost-user-fs device, mounted natively by the guest (`mount -t virtiofs <tag>`). The original objection — virtio-fs carries FUSE session state outside QEMU, which historically made VMs unmigratable and blocked savevm-style online snapshots — expired with the QEMU ≥8.2 / virtiofsd ≥1.11.0 device-state transfer: vmlab runs virtiofsd with `--migration-mode=find-paths --migration-verify-handles`, so its session state (open handles included) rides the snapshot's migration stream. Validated: save under dirty FUSE state, online reload, and restore-much-later into a fresh QEMU + virtiofsd. Costs accepted: the VM's RAM moves to a shared `memory-backend-memfd` (pre-existing snapshots of that VM stop restoring — the RAM block is renamed), one daemon per share, and the guest needs a virtiofs client (profile capability flag `virtiofs`; Linux ≥5.4 kernels have it, Windows needs the virtio-win driver + WinFsp in the template).

**SMB, served by the daemon at the segment gateway**, remains the universal fallback and the `auto` choice whenever host or guest lacks virtiofs support. Each SMB share is exposed as `\\<gateway>\<share>` on the VM's segment:

- **No guest driver burden.** Windows speaks SMB natively — nothing extra in template builds. Linux needs only `cifs-utils` (kernel CIFS is ubiquitous).
- **`windows-legacy` works** instead of being excluded — SMB2 covers Windows 7/2008R2-era guests, and **SMB1 (NT1/CIFS) is supported for guests that predate SMB2** (XP/2003-era): `smb1 = true` on a share enables the SMB1 dialect *and* the auth relaxation those guests require (NTLMv1/LM acceptance — XP doesn't send NTLMv2 by default; conflicts with `transport = "virtiofs"`). Off unless asked for; irrelevant as a security concern on an isolated lab segment, which is the whole reason vmlab can offer what the rest of the world has rightly abandoned.
- **Zero device state in the snapshot** — pure network traffic; a restored VM's SMB sessions are stale TCP that the guest's SMB client transparently re-establishes; mounts persist.

Share contents are outside snapshot scope on both transports (§7.3).

**Access model.** vmlab generates per-lab SMB credentials automatically; a share is mappable only with its owning VM's credential, scoping shares to their declaring VM even on a shared segment. Authenticated NTLMv2 + SMB signing is the baseline — required anyway because current Windows hardening (guest-auth blocking, signing requirements on recent Windows 11) rejects unauthenticated shares. None of this is user-visible: credentials are plumbed by vmlab.

**Correction required by §19.2.** The agent's mounts run as the agent identity
and land in the global DOS-device namespace, so every session *sees* the drive
letters while each logon authenticates separately; today's fix is an
`HKLM\…\Run` hook, which a facade-minted logon never fires because it is not a
desktop session. **The agent must write the lab's share credential into each
logon it mints**, before spawning anything — otherwise an attached developer
finds the mapped drive visible and unopenable, reporting a wrong password. Only
SMB is affected.

**Guest mounting** is performed through the guest agent once the VM is ready:

- **Linux (virtiofs):** `mount -t virtiofs <tag> <guest_path>` — no credential, no network dependency.
- **Linux (SMB):** `mount -t cifs //<gateway>/<share> <guest_path>` with the generated credential.
- **Windows:** mapped via the SMB client with the generated credential. A drive-letter `guest` target maps directly; a folder-path target is realised as a directory symbolic link to the UNC path. Windows supports UNC targets with `mklink /D`; `mklink /J` junctions cannot target UNC paths.

**Server implementation.** No mature embeddable SMB *server* library exists in the Rust ecosystem (clients only, verified at time of writing), so this is the largest single engineering component the feature implies. Two permitted strategies behind the identical WCL/user surface:

1. **Embedded minimal SMB server in the daemon** — the design goal: SMB2 (negotiate/session(NTLMv2)/tree/create/read/write/query-directory/close + signing) plus the **SMB1/NT1 dialect for `smb1` shares** — a second, older protocol surface (different framing, NTLMv1/LM auth) that materially enlarges this component; no oplocks, no DFS. Self-contained, no dependencies.
2. **Bundled `smbd` as an interim backend** — the daemon generates config, runs Samba unprivileged on a localhost high port, and the switch proxies the segment gateway's port 445 to it. Samba still ships an SMB1 server behind explicit configuration (`server min protocol`), so this backend covers `smb1` shares from day one. Cost: a Samba dependency (a documented host package) — **⚠ verify at implementation** that the bundled/target Samba build retains NT1 support, since distros increasingly trim it.

The PRD permits shipping 2 first and replacing with 1 later — including a hybrid where the embedded server handles SMB2 shares and `smb1` shares route to smbd; the user surface must not change between strategies.

**XP-era caveat, stated honestly:** XP/2003-era guests carry the legacy agent over COM1 (§7.4), which serves `exec` and nothing else, so vmlab's agent-driven mounting applies to them through an exec'd `net use X: \\<gateway>\<share> /user:...` rather than through the file session a modern guest gets; the mount step is the same one the provision script would type. DOS guests have no SMB client worth automating, and stay screen-driven for whatever their template does with the network. The docs should include the XP mount as a worked example.

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

#### 9.1.1 Kernel fast paths (implemented optimisation)

The substitution seam above is exercised by two opt-in eBPF fast-path tiers, selected empirically at daemon startup and surfaced by `vmlab fastpath` (and a badge in the web UI):

- **afxdp** — VM NICs attach as tap devices (`-netdev tap,fd=`) with a per-segment XDP program forwarding known non-isolated guest unicast tap-to-tap in-kernel. Broadcast, gateway-addressed, unknown, and isolated traffic punts to the daemon over a tagged host tap and traverses the userspace switch as before.
- **sockmap** — the stream-socket attachment is kept, but sk_skb programs splice known guest-to-guest unicast between the QEMU sockets in-kernel; everything else passes to the daemon's readers unchanged. Functionally validated but *measured slower* than the userspace fabric (af_unix redirects ride the kernel's psock backlog workqueue), so `auto` never selects it — it exists for explicit evaluation via `fastpath = "sockmap"`.
- **userspace** — the fabric exactly as specified above; always the fallback.

The rootless guarantee is untouched: both kernel tiers need CAP_BPF + CAP_NET_ADMIN, and each daemon *proves* a tier works on its host (loading the programs and pushing frames through throwaway taps/sockets) before using it — an unprivileged or WSL2 daemon degrades to `userspace` silently. Lab semantics are tier-invariant by construction: only frames the daemon provably doesn't need to see (known unicast between two non-isolated guest ports of one segment) are ever forwarded in-kernel; the gateway MAC and service/trunk ports never enter the kernel forwarding tables. Selection can be forced with the `fastpath` host-config field (`auto`/`off`/`sockmap`/`afxdp`) or `VMLAB_FASTPATH`.

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
lab.machine("api") -> Machine        # any machine, error if undefined
lab.machines() -> [Machine]
lab.vm("dc01") -> Machine            # error if undefined, or if it's a container
lab.vms() -> [Machine]               # the VMs only
lab.container("web") -> Machine      # error if undefined, or if it's a VM
lab.containers() -> [Machine]        # the containers only
lab.this_vm() -> Machine             # the machine that declared this script
lab.segment("corp") -> Segment
lab.name() -> string
lab.log(msg)                         # into the lab log + live CLI stream
```

There is one machine handle, not one per kind (ADR-0002). `lab.vm` and
`lab.container` remain because they read well and because rejecting the other
kind's name gives a better error than "no such machine" — but they return the
same `Machine`, and every operation below is available on all of them.
`Vm` remains a silent compatibility alias for `Machine` so first-boot scripts
embedded in older templates continue to compile; new scripts document and use
`Machine`.

### 10.2 Segment handle

```
seg.block(cidr, opts)    seg.unblock(rule_id)
seg.redirect(from, to, opts)
seg.dns_set(name, ip)    seg.dns_sinkhole(pattern)   seg.dns_clear(...)
seg.route_to(other_segment)          # opt-in inter-segment routing, reversible
seg.forward(host_port, vm, guest_port) -> rule_id
seg.rules() -> [Rule]
```

### 10.3 Machine handle

Every operation is available on every machine. What a particular machine cannot
do is reported **at call time and names the capability** — `screenshot` on a
machine with no display fails with *"machine `api` has no display"*, never with
"no such method" and never with a claim about its kind. That is what keeps the
expansion point open: the day a container reports a display, the same script
works unchanged.

**Lifecycle / state**

```
m.start()  m.stop()  m.stop_force()  m.restart()  m.poweroff()
m.name() -> string   m.kind() -> "vm" | "container"
m.state() -> Running | Stopped | ...
m.wait_ready(timeout)                # fully usable: agent up, first-boot done
m.is_ready() -> bool   m.is_healthy() -> bool   m.agent_answering() -> bool
m.wait_shutdown(timeout)
m.ip() -> string   m.ip_nic(i) -> string     # from lease table / agent
m.logs(lines) -> string              # console log, where the machine keeps one
```

Inside a machine's **own first-boot provision**, `is_ready` / `wait_ready` mean
agent-level readiness rather than full readiness: full readiness is unreachable
until that script returns, and a first-boot script that reboots its guest needs
to wait for it to come back. Everywhere else they mean full readiness.

**Snapshots**

```
m.snapshot(name)                    # online or offline per current state
m.restore(name)                     # resumes running iff snapshot was online
m.snapshots() -> [SnapshotInfo]     # name, taken_at, power_state
m.delete_snapshot(name)
```

**Input** — the Display capability

```
m.send_keys("ctrl-alt-del")         # chords, QMP sendkey naming
m.type_text("Password1!\n", opts)   # human-ish pacing options
m.mouse_move(x, y)  m.mouse_click(button)  m.mouse_drag(...)
```

**Screen** — the Display capability

```
m.screenshot(path?) -> Image
m.wait_for_image(ref, opts) -> Match        # opts: timeout, threshold,
m.wait_for_any([refs], opts) -> Match       #   region{x,y,w,h}, interval
m.find_image(ref, opts) -> Match | None     # single-shot, no wait
m.ocr(opts) -> string                       # Tesseract; optional region
m.wait_for_text(pattern, opts) -> Match     # OCR-based wait, regex pattern
```

Reference images are paths relative to the lab (convention: `images/` beside `vmlab.wcl`). Matching is normalised template matching with a similarity threshold (default ~0.9, overridable). `Match` carries location + score, so a found image can anchor a relative mouse click.

**Guest agent**

```
m.exec(cmd, opts) -> ExecResult     # exit_code, stdout, stderr; timeout opt
m.copy_to(local, guest_path)
m.copy_from(guest_path, local)
m.logins() -> [Login]               # the machine's declared logins (§19.2)
```

`opts` on `exec` carries `user` (and `password`, for an account the lab file
never declared), the wscript rung of §19.2's precedence ladder. Reading the
declared logins is what lets a provision script create *exactly* the account
declared, rather than the password existing in two places that drift.

All blocking calls take timeouts and return wscript `Result`s; an error propagating out of a provision script fails the provision run (and therefore `vmlab up`) with the error attached to the lab log.

### 10.4 Execution model

- Provision scripts are declared inside the `vm {}`/`container {}` they belong to and run during `up` in declaration order, once that machine is ready (per `depends_on`). A script orchestrating multiple machines (stand up DC → wait → join member) is the expected normal case — it reaches the others through the lab handle, and `lab.this_vm()` gives it the machine that declared it, VM or container alike.
- Any script is also invocable ad hoc: `vmlab script scripts/whatever.ws` (no owning machine, so `this_vm()` is unavailable).
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
| `vmlab status [-v]` | Machine status, IPs and segments, plus `dev` and `attachable` (§19.4); `-v` adds raw state and per-kind detail |
| `vmlab validate` | Full §5.1 validation, no side effects |
| `vmlab vm start / stop / restart <vm>` | Per-VM power control |
| `vmlab snapshot create / restore / list / delete` | Per-VM or lab-wide (§7.3) |
| `vmlab console <vm>` | Attach viewer |
| `vmlab exec [--timeout s] <vm> -- cmd` | Guest-agent exec |
| `vmlab cp <src> <vm>:<dest>` | Copy a host file/tree into a guest via the agent |
| `vmlab osinfo <vm>` | Guest OS identification as JSON |
| `vmlab script <script.ws>` | Ad-hoc script against the current lab |
| `vmlab logs [lab/][vm]` | Tail/dump JSON-line logs |
| `vmlab ssh <machine> [-- cmd]` | Attach over the SSH facade — refreshes the managed block, then `exec`s the system `ssh` (§19.7) |
| `vmlab ssh-config [--print <m>]` | Refresh the managed `~/.ssh/config` block; `--print` emits a stanza plus the editor settings snippet (§19.7) |
| `vmlab dev attach [machine]` | Up, wait for `attachable`, become a shell on the dev machine (§19.7) |
| `vmlab dev use <machine>` | Record which dev machine is *mine*, in the lab's `.vmlab/` (§19.7) |
| `vmlab dev sync status / flush / diff / resolve` | Workspace syncer state and conflict resolution (§19.6) |
| `vmlab machine capabilities / stats` | Per-machine probed capabilities, including `attachable` (§19.4) |
| `vmlab machine repair-agent <machine>` | Push the shipped agent into a running machine and mark it diverged; never automatic (§19.4) |
| `vmlab template build / list / rm / export / import` | Template store |
| `vmlab template push / pull / login` | OCI registry distribution (§6.4) |
| `vmlab daemon start / stop / status` | Supervisor control (normally automatic); status lists lab daemons |

---

## 13. WSL2 considerations (summary)

Everything above was chosen to be WSL2-clean, but to state it once: KVM requires nested virtualisation enabled in `.wslconfig`; networking uses no tap/bridge/macvlan so no WSL kernel or privilege gymnastics; host access from Windows rides port-forwards + WSL localhost forwarding; `$XDG_RUNTIME_DIR` must be verified/created at daemon start (some WSL setups lack it); and the disk-space watchdog matters more here because the ext4 VHDX grows.

---

## 14. Official container image

**Removed from scope before the first release.** vmlab no longer ships a
Docker/OCI runtime image, a Containerfile or a Compose stack; the CLI is the
only deliverable, and a native install beside the host's QEMU (plus the
runtime tools the CLI reports missing by name) is the supported path. The
guarantees the image used to be the proof of still hold on any host: `/dev/kvm`
is the only grant KVM acceleration needs, TCG is the loud-warning fallback
without it, and the default network fabric needs no privileges at all (§1.1,
§13). Template artifacts in OCI registries (§6.4) and `container {}` machines
(§18) are unrelated to this image and remain.

---

## 15. Suggested milestones

1. **M1 — Core lifecycle:** supervisor + lab daemon split with socket protocol, WCL schema + validate, template store (import existing qcow2 only), linked clones, start/stop, QMP, guest-agent exec/copy, single NAT'd zero-config segment, logs.
2. **M2 — Automation surface:** wscript host module (lifecycle, exec, keys, screenshot, image match, waits), provision scripts, `run`, snapshots both modes.
3. **M3 — Network fabric:** named segments, DHCP + reservations + option 121, DNS + registration + forwarding, port forwards, console/VNC.
4. **M4 — Template builds + shares:** ISO sources w/ URL+hash, media building, build scripts, export/import, profiles complete incl. legacy, SMB shared folders (smbd backend acceptable initially per §7.5).
5. **M5 — Advanced networking + events:** global segments, cross-host attach, inter-segment routing, filtering/redirection + runtime mutation, event handlers, watchdogs, OCR.
6. **M6 — Distribution:** OCI push/pull with chunking and multi-arch indexes, registry auth, lab references to registry templates. (The official container image was later dropped, §14.)

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
`healthcheck {}`, and `nic {}` blocks identical to VM NICs.
VM and container names share one namespace: DNS, `depends_on` waves,
`forward { to = "name:port" }` targets, and configuration steps all resolve
across both kinds. A container with no NICs is valid — air-gapped, still
reachable via `exec`/`cp` over the agent channel — unless it declares
volumes, which are network-mounted (validation error, mirroring §7.5).

**Architecture.** Every container runs in a **micro-VM**: a pinned Alpine
`linux-virt` kernel + purpose-built initramfs (`vmlab-cinit` as PID 1,
spawning `vmlab-agent`), booted directly with `-kernel/-initrd`. Its `cpus` and
`memory` resolve through the §5.2 chain — the container block, then its
`profile` (there is no template layer) — and there is no built-in default,
because what a micro-VM needs depends entirely on its image: a container that
neither declares a size nor names a profile supplying one is a §5.1 validation
error rather than a guess it silently OOMs under. The shipped `container`
profile carries the conservative floor of 1 vCPU / 256 MiB. The image's layers
are flattened (whiteout-aware, tar-level, no host privileges) into a squashfs
mounted read-only, with a per-container
scratch qcow2 as the overlayfs writable layer. Config reaches the guest as a
`ContainerSpec` pushed over the ctl channel (the virtio-serial port that also
carries lifecycle events and stop/resync commands). Volumes attach as
**vhost-user-fs devices** — one `virtiofsd` per volume, spawned by the lab
daemon with `--migration-mode` so its FUSE session state rides the
snapshot's migration stream — mounted natively by cinit (`mount -t
virtiofs`) before the network is even up; a volume-carrying micro-VM's RAM
switches to a shared `memory-backend-memfd` (a vhost-user requirement).
Hosts without a `virtiofsd` binary fall back to the v1 transport: SMB
shares served by the lab daemon at the segment gateway — the same
bundled-`smbd` mechanism as §7.5 shared folders — mounted by cinit over
CIFS once the network is up. No 9p device is ever attached (it would add a
migration blocker and break online snapshots, §7.3). Ownership on volume
files is mount-level, not per-file container uid/gid. This preserves the
rootless baseline (§1.1, §13): containers need **no** `--privileged` or added
capabilities — `/dev/kvm` only, with TCG fallback without it. The optional
eBPF network fast path uses its own narrowly scoped grants.

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
`depends_on` waves. Events `container.starting/ready/stopped/crashed/unhealthy`
are bindable with `on {}`. The stop ladder mirrors VMs: in-guest stop signal +
grace, then guest shutdown, then kill. Container stdout/stderr is the serial
console log (`vmlab container logs [-f]`).

**Snapshots.** Containers are snapshottable with full VM parity (§7.3).
Offline snapshots capture the scratch disk; online snapshots capture scratch
+ RAM + device state, both as qcow2-internal snapshots on the per-container
scratch qcow2 (the read-only rootfs squashfs is immutable and outside the
snapshot). The power state recorded at capture drives restore exactly as for
VMs — an online snapshot resumes the container mid-flight, process state and
all — and lab-wide snapshot verbs cover containers alongside VMs. Volume
contents are host state outside snapshot scope, exactly like §7.5 shares —
restore never rolls back volume files. virtiofs volumes survive restore
natively: `virtiofsd` transfers its session state through the snapshot
(open handles included), validated for online reload and
restore-much-later into a fresh daemon. On the CIFS fallback, like a
restored VM's SMB session the volume's session is stale TCP the client
re-establishes (the micro-VM mounts with `echo_interval=5`, so the first
post-restore volume access stalls seconds, not minutes). Each snapshot
records the container's
pinned image digest; restoring under a different pin fails (the writable
layer is only valid against the same rootfs).

**Deliberate exclusions.** Named volumes are lab-scoped, shared
by name, and survive `down` and per-container destroy; only lab `destroy`
removes them. Cross-arch containers (image arch ≠ host arch) are out of scope
for v1, as is a crun/native runtime backend (recorded as a possible fast path
if micro-VM start latency ever matters).

---

## 19. Dev machines

Any lab machine — VM or container, Windows or Linux — may be designated the
lab's **development environment** with a `@dev` decorator. vmlab publishes it as
an **SSH endpoint** that any SSH-capable editor attaches *into*: the language
server, the build, the debugger and the terminal all run guest-side, against
real guest paths, the real toolchain and — where the lab has one — the real
domain identity. Host-edit/guest-execute was considered and rejected; the editor
goes in.

The bar is **parity, not novelty**: what devcontainers give a Linux developer,
for a Windows application on a real domain. Nothing else serves that case, which
is why it is the case that has to work.

Two things are specified here and only one of them is dev-specific:

- **The SSH facade** (§19.3) is a *general* capability of every machine.
  `vmlab ssh mem01` works on a machine carrying no `@dev` at all. It lives in
  §19 rather than a section of its own because §19 is why it exists.
- **The dev machine** is the workspace (§19.6) and the verbs that are
  meaningless without one (§19.7).

**One contract, every machine kind.** Where the two guest families differ, the
difference is absorbed by the **profile** (a default workspace path) or by the
identity floor (§19.2), never by a branch in the mechanism.

### 19.1 The declaration

```
lab "probe" {

  segment "corp" { subnet = "10.77.0.0/24" dns { server = "10.77.0.10" } }

  vm "dc01" {
    template = "x86_64/windows-server-2025"
    nic { segment = "corp" ip = "10.77.0.10" }
    provision "provision/dc.ws" { }          # creates PROBE\dev, PROBE\admin
  }

  @dev(default = true, workspace = "./src", workspace_guest = "C:\\src")
  vm "dev01" {
    template   = "x86_64/windows-server-2025"
    depends_on = ["dc01"]
    nic { segment = "corp" }

    login "dev"   { user = "PROBE\\dev"   password = "vmlab123!" default = true }
    login "admin" { user = "PROBE\\admin" password = "vmlab123!" }

    provision "provision/join-and-tools.ws" { }
  }

  @dev(workspace = "./src")
  container "buildbox" {
    image = "mcr.microsoft.com/dotnet/sdk:9.0"
    cpus = 4  memory = 4GiB
    nic { segment = "corp" }
  }
}
```

**`@dev` is a decorator, not a child block**, because it states something
*about* the machine rather than configuring something *inside* it — nothing it
carries is a setting the guest ever sees. A lab-level `dev { machine = "…" }`
was rejected on §5.1's existing rule that configuration is declared inside the
machine it targets, so there is nothing to cross-reference.

**Cardinality is many, and zero is normal.** Any number of machines may carry
`@dev`; most labs are not dev labs. The **default dev machine** is the one
carrying `default = true`, or — where none does — the only machine carrying
`@dev`. A lone dev machine therefore writes a bare `@dev` and never meets the
concept. "First in file order wins" was rejected: declaration order already
means something in vmlab (§5.1), and overloading it would let a block reorder
silently move the default. Note that `default = true` is the **lab file's**
choice, so it is the same for everyone who clones the repo; which dev machine is
*mine* is host-side state (§19.7).

**Arguments are admitted by a three-part rule**, not by a list. An argument
earns a slot on `@dev` only if it is all three of: **per-machine** (it differs
between two dev machines in one lab), **not already expressible** in an existing
block, and **a declaration the lab author owns** — not a per-developer choice
and not host-side state. The rule, not the current list, is the contract.

Ruled out by it, each for a stated reason rather than taste:

- **Editor hints** (`editor = "vscode"`). The SSH endpoint is the whole
  contract, and vmlab deliberately does not repeat devcontainers' coupling to
  one editor. The editor settings snippet §19.8 describes is something vmlab
  *hands* the developer, never something the lab file declares.
- **Ports.** Container `port {}` and §9.8 forwarding already express them. A dev
  machine's ports are ordinary ports, and they are **declared, not discovered** —
  vmlab does not watch a guest for listening sockets. `ssh -L` over the facade's
  `direct-tcpip` is the ad-hoc path.
- **Toolchain and package lists.** That is `provision {}` / `playbook {}`
  (§19.4). `@dev` never grows a list duplicating config-weave.
- **Ignore rules and dotfiles.** Developer-owned; they live in the repo tree
  (§19.6) and in the developer's own tooling (§19.8).
- **Identity.** It outgrew the decorator: the facade is general, so
  `vmlab ssh mem01` needs an answer on an unmarked machine too. Identity is
  declared machine-level (§19.2).

That leaves `default`, `workspace` and `workspace_guest`.

**Every argument is optional, and a bare `@dev` is a complete, attachable dev
machine.** Unset arguments resolve **`@dev` argument > profile > vmlab floor**,
with a hard error where nothing supplies a value and no floor is sensible — the
call §18 already made for container `cpus`/`memory`. Defaults are
profile-sourced because they are guest-OS-shaped (a workspace path is `C:\src`
on Windows and `/src` on Linux), and `profile_schema.wcl` already carries
non-hardware behaviour keys. Two guardrails: this resolver is **separate from
ADR-0008's**, which is *the* route for hardware precedence and must not be
muddied by something that is not hardware; and a profile with no dev keys still
hosts a dev machine — a missing key means "the floor applies", never "this
profile cannot be a dev target", or `@dev` on the `custom` profile would be
impossible.

**`@dev` is projected and designable like any other part of the schema**
(ADR-0005). WCL's decorator-schema introspection returns declarations in the
same shape `projection.rs` already consumes for block fields, so the rendered
reference reaches it with no special case.

**Validation.** The decorator's own errors — an undeclared `@dve`, a wrong-typed
or unknown argument, `@dev` on a `nic {}`, a repeated `@dev @dev` — come from
WCL, which validates instance decorators against their declaration and their
declared applicability (`@applies_to(on = [:block], kinds = ["vm",
"container"])`). What WCL cannot see is the cross-block case, so §5.1 gains one
rule for `@dev`: **more than one `@dev(default = true)` in a lab is an error,
naming both machines** — the same class as the existing duplicate-static-IP
rule. `@dev` on a machine whose agent cannot serve an attach is *not* a
validation error; see §19.4 for why, and where it fails instead.

> **✔ Prerequisite, met.** Validated instance decorators were WCL work that
> landed after the `wcl_lang` revision vmlab pinned. The pin now carries them,
> and the schema projection reflects decorator declarations alongside block
> fields, so `@dev` is ordinary schema work.

### 19.2 Identity — who you are when you attach

**The machine declares who you are, and vmlab logs you on as them.** Identity is
a property of the machine, not of the attach, and is declared with a repeatable
`login "<label>" { user, password?, elevated?, default? }` block on `vm` and
`container`. A child block rather than flat fields because the account and its
secret are meaningless split, and because `elevated` needs somewhere to land.

**The SSH username is the selector, and it carries the *label*.**
`ssh vmlab-probe-dev01` attaches as `dev`; `-l admin` as `admin`. The raw
account name is accepted as an alias for its label, but the label wins. Three
things follow: one account may be declared twice at different elevation;
`DOMAIN\user` never has to survive an SSH username or `ControlPath`'s `%r`; and
the generated stanza's `User` line is a label vmlab chose.

**The secret goes in the lab file, plainly.** The account exists because the
lab's own provisioning created it, so the same string is already sitting in
`provision/dc.ws` in the same repo — in a synthetic lab the password *is*
lab-author-owned, which is exactly how it passes §19.1's third clause. This is a
decision about what a lab *is*, and it buys the absence of a great deal: **no
credential store, no login verb, no wscript credential API, and no second WCL
dependency** (WCL has no environment or file interpolation, so "declare it but
do not commit it" was never available). Consistent with §1.2 — vmlab is not a
security boundary.

**Precedence: CLI flag → wscript → `login {}` → agent identity.** `vmlab ssh
dev01 -l admin`; `--user`/`--password` on `ssh`/`exec`/`shell` for an account
the lab file never declared; wscript can both *read* the declared login (so a
provision script creates exactly the account declared, rather than the password
existing in two places that drift) and override it. A second ad-hoc identity is
a flag, not a schema addition.

**Everything a person invokes defaults to the declared login; everything vmlab
does on its own behalf keeps the agent identity.** Person-invoked: `vmlab ssh`
and the facade, `exec`, `shell`, `push`/`pull`. vmlab's own: `provision {}`/`playbook {}` steps, share mounting,
readiness, metrics, tail, shutdown. The dividing argument is the bootstrap and
it is decisive rather than stylistic — `PROBE\dev` does not exist until
provisioning creates it, so a lab whose provisioning ran as the declared login
could never stand up its own domain, which is the case this section exists for.

> **The rule, with its one exception: vmlab's own machinery runs as the agent
> identity, except where it produces the developer's files.**

The exception is the workspace syncer (§19.6), which writes as the machine's
default login. Otherwise it writes `C:\src` as SYSTEM and the developer owns
none of their own source tree — build outputs beside SYSTEM-owned files, saves
hitting ACL errors on some files and not others, `.git` objects owned by a
principal the user is not. Two consequences: the syncer **starts after
provisioning**, not at machine-ready, and it shares the developer's cached
logon, so a file created by sync and one created in the shell are
indistinguishable. With no `login {}` declared the floor applies and the tree is
SYSTEM/root-owned — still correct, because the attached session is SYSTEM/root
too. **Ownership always matches whoever will attach.**

**The floor needs no new spelling.** `--user SYSTEM` on Windows and `--user
root` on Linux *are* the agent's identity, so the facade treats them as "spawn
directly, no logon". Stated plainly because it is a real break: on a machine
that declares a `login {}`, `vmlab exec` and `vmlab push` **stop being
SYSTEM/root**, and pushing into `C:\Windows\System32` starts failing where it
used to work. Only machines that opted in are affected, and it is what makes "I
am the dev user on this box" true in every verb rather than in one.

**Elevation is declared, defaults to `true`, and is Windows-only.** An editor
invokes `ssh <alias>` and nothing else, so elevation must be selectable through
something SSH carries — hence a field on a *labelled* block rather than a flag.
It defaults true because the parity bar is devcontainers and a devcontainer
gives you root; `elevated = false` serves the real but rarer "test as a standard
user" case, and §19.6 names the two ways it degrades the workspace. Without the
field a **local** admin would land filtered under
`LocalAccountTokenFilterPolicy` while a **domain** admin would not — an
invisible distinction to declare against. An elevated session is a deviation
from what a real interactive logon gives; §19 says so rather than leaving it
implied.

**Windows mechanics — four requirements, each of which silently breaks something
if missed.** All four were settled against a live offline domain (a Server 2025
DC plus a domain-joined member), not inferred:

1. **`LogonUser` with `LOGON32_LOGON_NETWORK_CLEARTEXT`.** The agent runs as
   LocalSystem and mints a real logon, which carries a **real initial TGT**
   (`initial pre_authent`) and genuine network credentials — unlike the
   `KERB_S4U_LOGON` identity-without-credentials a key-authenticated Windows
   sshd produces, which is the finding that moved the SSH server to the host
   (§19.3). `BATCH` and `SERVICE` are refused outright (1385) and `INTERACTIVE`
   is refused **on a domain controller**, where "log on locally" is not granted
   to ordinary users — so choosing `INTERACTIVE` would quietly make "the DC is
   my dev machine" impossible. `NETWORK_CLEARTEXT` works on both machine kinds,
   trips no policy on either, and still yields the full TGT.
2. **`LoadUserProfileW` before `CreateProcessAsUserW`.** It *creates* the profile
   on demand for a never-logged-on domain user. Skip it and `USERPROFILE` is
   `C:\Users\Default` — shared, wrong, and silent, with every editor that writes
   under `$HOME` scribbling into it.
3. **`AdjustTokenPrivileges`.** SYSTEM holds `SeAssignPrimaryToken` and
   `SeIncreaseQuota` **present but disabled**; `CreateProcessAsUserW` fails
   until the agent enables them.
4. **The linked token** where the account has one, for `elevated = true`.

**A Linux session is a real login, not a `setuid`.** The cheap implementation
gives the right uid and nothing else — no `XDG_RUNTIME_DIR`, no PAM, no systemd
user session, no keyring — which surfaces as rootless podman failing while
`$HOME`-relative things work. The session must be indistinguishable from having
logged in: `HOME`, `USER`, `LOGNAME`, `SHELL` and supplementary groups from the
passwd entry, cwd at `HOME`, a login shell, and `XDG_RUNTIME_DIR` where the
guest has logind — realised through the guest's own login machinery (so PAM
runs) where it exists, plain `setuid` where it does not. That is the same
standard `LoadUserProfileW` sets on Windows.

**Floors differ by machine kind and nothing else.** A VM with no `login {}`
falls to the agent identity. A container falls to the user cinit already
resolves: the declared `user`, else the image's `USER`, else root — which is
devcontainers' `remoteUser`/`containerUser`, and costs nothing because Linux
needs no credential to become that user.

**The cached logon** is keyed on **(account, secret, machine)** — not on the
label, so two labels naming one account share a session, and a changed password
mints a fresh logon rather than failing against a stale token. It lives while
any channel uses it plus a bounded idle grace aligned with the alias's
`ControlPersist`, is **recycled at idle once older than its Kerberos ticket
lifetime** (a dev box left up over a weekend would otherwise wake holding a
logon whose TGT expired days ago, surfacing as "the share stopped working" with
no visible cause), and **never survives the machine stopping**.
`LoadUserProfileW` pairs with it — loaded when minted, unloaded when dropped, or
the user's registry hive stays mounted for the machine's life. One measured
logon costs ~97 ms, so attach latency is a non-issue.

This makes **"the SFTP logon is the same logon" true by construction rather than
by discipline**: the facade's file operations resolve the same (account, secret)
as the shell, so they land on the same cached logon, the same `LogonId` and the
same view of mapped drives. Verified — three processes from one cached token,
one `LogonId`, one ticket cache.

**The lab's share credential must be injected into each minted logon**, before
anything is spawned. This is a **correction to §7.5**, not an addition: the
agent's own mounts run as SYSTEM and land in the global DOS-device namespace, so
every session *sees* the drive letters while each logon authenticates
separately. The existing fix is a `Run`-key hook, and a facade logon never fires
one — a `Run` key needs a desktop session, and `NETWORK_CLEARTEXT` +
`CreateProcessAsUserW` is not that. Without the injection an attached developer
lands in exactly the documented failure: `Z:` is visible and opening it says the
password is wrong. Only SMB is affected; virtiofs mounts through a service-owned
global device with no credential, and Linux mounts are global in a shared
namespace.

**vmlab never creates a guest account.** That is `provision {}`/`playbook {}`'s
job by §19.1's second clause, and it costs nothing in the flagship case anyway:
a member server's SYSTEM has no domain rights, so vmlab *cannot* create
`PROBE\dev`. Creating local accounts only would be a half-rule that works on one
machine and not the next.

**Failure is loud, never a fallback.** A declared account that does not exist, or
a wrong secret, fails naming the account and the machine. Falling back to the
agent identity would leave commands mysteriously running as SYSTEM and writing
into `systemprofile`.

**§5.1 gains three rules:**

> - `login { user = … }` with no `password` on a **Windows-family** profile is a
>   validation error — the agent is SYSTEM and every credential-free route is
>   the one Windows OpenSSH's S4U logon already disqualified.
> - `elevated` on a **Linux-family** profile is a validation error — root is
>   root, and a non-root user is not elevatable without sudo.
> - More than one `login` with `default = true` on a machine is a validation
>   error, naming both.

A lone `login {}` is the default implicitly, matching `@dev`'s shape.

### 19.3 The SSH facade

**vmlab terminates SSH on the host. The guest runs no sshd at all.**

```
editor ──ssh──► vmlab SSH facade (in labd)
                    │ maps SSH channels to agent channels
                    ▼
            vmlab.agent.0 ──► vmlab-agent (LocalSystem / root)
                    │ LogonUser + CreateProcessAsUserW
                    ▼
            the developer's process, with real network credentials

guest: no sshd, no host keys, no authorized_keys, no NIC required
```

A tunnel to a guest `sshd` sidesteps *none* of the Windows OpenSSH findings —
sshd would still do the authenticating, so the profile-path trap, the
`administrators_authorized_keys` redirect, the `cmd.exe` default shell, the
cloned host key and, fatally, the missing network credentials all survive a
change of transport. So the SSH server moved to the host, where the agent's
LocalSystem context can mint the one thing that carries network credentials
(§19.2). **This is the load-bearing decision of the whole section; everything
else follows from it** (ADR-0012).

**Transport is the agent channel.** No NIC, no DHCP lease, no NAT priming, no
host port — identical for VMs and container micro-VMs, and a machine
deliberately cut off from every segment can still be attached to. Guest
networking was rejected: a §9.8 forward rides the machine's first NIC's segment
and is skipped until a lease exists, and it fixes none of the above regardless.

**The endpoint is a stdio `ProxyCommand` and nothing else.** The proxy process
*is* the client's server connection: one per `ssh`/`scp` invocation, speaking
SSH over stdin/stdout. **Nothing listens on the host and no port is leased.**
The SSH implementation lives in `labd`, beside the agent client, the cached
logon and the feature probe; the proxy is a byte pipe over **one lab command
that returns a unix socket path**, so a proxy invocation costs a `connect(2)`
plus a copy loop. Precedent is in tree — `machine.terminal` already re-exposes
an agent session as a raw-byte unix socket, and the console's VNC bridge does
the same. Rejected: the proxy itself terminating SSH and driving channels over
the lab protocol, which would re-export agent-proto through ADR-0007's typed
vocabulary and make every one of the several `ssh`/`scp` processes a client
spawns pay for it.

**Auth is `none`, and vmlab owns the host key.** There is no network path to the
facade, so the trust boundary is already "can you exec the proxy against this
lab socket". Nothing to generate, store, rotate or leak. A per-machine host key
lives in vmlab's own state directory with vmlab's own `known_hosts`, so the
developer's `~/.ssh/known_hosts` is never touched and a rebuilt machine never
triggers a host-key warning. Observed against a real client: OpenSSH's opening
`none` probe is **unconditional** — it is how the client enumerates methods — so
`PreferredAuthentications`, `BatchMode`, `NumberOfPasswordPrompts=0` and
`PasswordAuthentication=no` all still authenticate. `none` cannot be talked out
of.

**What the facade answers:**

| Request | Serviced by | Because |
|---|---|---|
| `pty-req`, `shell` | `OpenTerminal` | plain `ssh`, an IDE's terminal |
| `exec` | `OpenExec` | VS Code's PowerShell bootstrap |
| `window-change` | `Resize` | |
| `subsystem sftp` | host-side SFTP over `fileops` | `scp`, the editor server push |
| `env` | applied over the logon's environment, minus a deny-list | most distros ship `SendEnv LANG LC_*` |
| `direct-tcpip` | `OpenTunnel` | VS Code rides its whole protocol over `ssh -T -D` |
| `exit-status` | always sent, from the agent's exit code | `ssh`/`scp` exit codes depend on it |

`env`'s deny-list is load-bearing rather than defensive: `HOME`, `USERPROFILE`,
`USERNAME`, `LOGNAME` and `SSH_AUTH_SOCK` are dropped, because a client-sent
`USERPROFILE` would silently undo the `LoadUserProfileW` that gave a
never-logged-on domain user a profile at all. Dropped, not an error — the
request is best-effort by design. `exit-signal` is **never sent**, because the
agent reports `128 + signal` rather than a signal name; `ssh devbox 'kill -9
$$'` therefore reports status 137, which is the honest translation of what the
agent knows. At the connection level, `keepalive@openssh.com` gets
`SSH_MSG_REQUEST_FAILURE` — which *is* the correct answer, and is what makes
`ServerAliveInterval` work — `no-more-sessions@openssh.com` is accepted and
ignored, `hostkeys-00@openssh.com` is never advertised (one key per machine,
nothing to rotate), and many `session` channels per connection are expected,
since `ControlMaster` exists to put them there.

**`direct-tcpip` is mandatory, not a convenience.** VS Code runs `ssh -T -D
<port>` and rides its entire protocol over that SOCKS forward, so refusing
`direct-tcpip` would not degrade the editor, it would break it. That promotion
rests on VS Code's **observed** channel; JetBrains Toolbox was seen asking for
`-D` with the same shape, which corroborates it but is not a second observation.
`-T` was observed on both, making "no `pty-req` ever reaches the control
connection" a property of remote-dev clients generally.

**What it refuses, and the invariant that decides it:**

> **The facade only ever answers a channel open; it never initiates one.** Every
> stream is client-initiated: `session` and `direct-tcpip`, nothing else.

This is not stylistic. It is agent-proto's own asymmetry — the host opens
channels and the guest answers, and there is no guest-initiated channel open in
the protocol (ADR-0013). `forwarded-tcpip`, `auth-agent@openssh.com` and `x11`
are channel types the facade never opens, which is precisely why
`tcpip-forward`/`cancel-tcpip-forward` (`ssh -R`),
`auth-agent-req@openssh.com` and `x11-req` are refused. Each would need a
guest-side listener with its own lifetime and bind policy against a
`ControlPersist` mux that outlives its client. §19 states the rule and names the
refusals as its consequences rather than enumerating requests a future reader
must keep extending. Everything else — `subsystem <other>`, `signal`, `break`,
`xon-xoff` — is refused because nothing in the client set sends it.

**How a refusal reads.** Only a channel **open** failure can carry vmlab's own
words (`SSH_MSG_CHANNEL_OPEN_FAILURE` has a description string). A channel
*request* refusal is `SSH_MSG_CHANNEL_FAILURE` and a global request refusal is
`SSH_MSG_REQUEST_FAILURE`, neither of which carries text — so §19 says plainly
that those refusals are **narrated by the client, not by vmlab**, rather than
promising a friendly message the protocol cannot carry. Observed at default
`LogLevel`: `-R` warns, `-X` warns, and **agent forwarding is refused in total
silence** — `SSH_AUTH_SOCK` is simply empty in the guest, so a developer
forwarding a key gets no signal and a later, unrelated-looking auth failure.
That is the one refusal worth spending vmlab's own words on, in the banner below
or in a line from `vmlab dev attach`.

Three further behaviours:

- **An unrecognised login label gets a `USERAUTH_BANNER` naming the machine's
  declared logins**, then an auth failure. The username is a selector over
  declared identities (§19.2), so an unrecognised selector is not an identity and
  auth is the right layer; the banner is the one place before it where free text
  is displayed. `none` is unchanged for a recognised label — the banner path is a
  refusal, not an authentication.
- **The facade degrades per channel.** An agent missing `fileops` still serves a
  shell (`terminal` and `exec` are baseline) while `subsystem sftp` refuses **by
  name**, telling the developer to rebuild the template or run the agent repair
  verb; `direct-tcpip` refuses the same way for a missing `tunnel`. §19.4's
  *hard at attach* belongs to `vmlab dev attach`, which owns a terminal — not to
  a proxy whose stderr an editor swallows.
- **Refused channels reach the lab event log**, which is otherwise the one place
  a refusal is not visible only in one developer's terminal.

**`ControlMaster` goes in the generated alias** (§19.7), backed by the per-user
cached logon: clients spawn several `ssh`/`scp` processes per session and each
would otherwise cost a handshake *and* a domain logon. Multiplexing through the
facade was observed working under a real client — two connection attempts, one
proxy invocation, one facade connection, two session channels.

**Throughput forks nothing.** Measured over `vmlab.agent.0` into a Windows
guest: 80 MiB/s sustained on a 1 GiB push, 141–185 MiB/s in bursts, and `exec`
round trips going 45–91 ms → 59–111 ms while ~1 GiB is in flight on the same
port — a latency bump, no starvation, no stalls. The frame and window constants
stay as they are and the mux needs no fairness fix.

**One coupling that is a requirement, not an implementation detail: the facade
must never grant SSH window it cannot back with agent credit.** There are two
stacked flow-control layers, and the naive implementation ACKs the client
generously and buffers the difference *inside `labd`* — which, against the
tens-of-megabytes editor-server push, is an unbounded buffer in the lab daemon.

### 19.4 What the guest must already have

**Two things, and only two: the agent, and the toolchain.** There is no sshd to
install, and the workspace path is created by the syncer rather than
pre-installed. The image is otherwise a stock template.

**The toolchain is the lab author's `provision {}` / `playbook {}`, and the
declaration grows no package list.** The instinct to add one was worth testing,
because what actually makes devcontainers usable is not `postCreateCommand` but
**features** — and "write a provision script that installs VS Build Tools" is
not obviously parity with that. It survives because vmlab's equivalent already
exists and is stronger on that axis:

> **A distributable template (§6.4) is vmlab's answer to devcontainer
> *features*.** A `windows/dotnet-dev` template pushed to a registry is
> installed once at build time and pulled by every developer, instead of
> re-running a feature install on every rebuild; playbooks are the declarative
> convergence surface for the remainder.

That equivalence is stated here explicitly so it is not relitigated during
implementation.

**Two further preconditions of a dev-capable image**, which join the agent and
the toolchain rather than being worked around silently: the guest must be
**symlink-capable** (`SeCreateSymbolicLinkPrivilege` or Developer Mode on
Windows), and a full Linux VM's own kernel must be recent enough that `inotify`
survives an overlayfs copy-up — container micro-VMs are covered by the kernel
vmlab itself pins, so this can only bite on a VM.

**The agent gains three capabilities, advertised as feature strings in its
handshake:**

| Feature | Serves |
|---|---|
| `tunnel` | the facade's `direct-tcpip` |
| `fileops` | the offset-addressed file vocabulary backing host-side SFTP |
| `watch` | the recursive guest tree watch backing the workspace syncer |

`tunnel` and `fileops` are named separately rather than as one coarse `ssh`
because they have independent consumers — `vmlab cp` has no interest in
tunnels. `watch` lives in **the agent**, not a second guest binary:
a separate daemon would be a second thing to bake into every template, a second
install path, a second skew axis and a second thing the repair verb must know
about, for code that has to sit on the same channel anyway. **User-logon spawning
gets no feature string** — the existing set is consumer-shaped, one per thing a
user does, and a logon is a modifier on how sessions are spawned rather than
something the agent serves.

**`PROTO_VERSION` stays 2**, on a structural argument rather than a
compatibility one. The host refuses a mismatched handshake **before any channel
opens**, and the repair verb below pushes a binary *over the agent channel* — so
a bump would make the agent unrepairable on exactly the machines that need
repairing, with no fallback, because the only other execution path into a guest
is screen keystrokes. A bump would also be a blackout rather than a degradation:
v2 is baseline for readiness, IP discovery and the shutdown ladder, so a v3
requirement would take `exec`, `terminal` and *ready* with it. Feature-string
degradation is the precedent already in tree.

**Rebuild is policy; repair is a tool — and this paragraph is a *VM*
statement.** The agent enters an image exactly once, at build (§6.1, §7.4), so a
stale agent is a rebuild. Alongside that, a **machine-scoped verb pushes the
host's shipped agent binary into a running machine on demand and marks that
machine `diverged`**. It never fires by itself: an automatic refresh at `up`
would make the template's sealed `agent_version` a lie and stop *same template →
same machine* holding. It exists because a 15–45 minute Windows rebuild to pick
up an agent change is otherwise the inner loop of building §19 itself. It sits
under `vmlab machine`, beside `capabilities` and `stats` (§12).

None of that applies to a container. **A container micro-VM's agent lives in the
initramfs guest asset**, not in any image, so it tracks the host's installed
vmlab and cannot go stale; the repair verb is meaningless there, and refreshing
means reinstalling the guest asset. `attachable` therefore reduces to "is the
host's vmlab current" for a container and stays a genuine per-machine probe only
for VMs.

**Where it fails: silent at `validate`, warn at `up`, hard at attach.**

- **`validate` says nothing.** It is a config check with no side effects, and the
  only statically available signal is the template's sealed `agent_version` — a
  free-form string. Comparing it is *inference*, which the capability doctrine
  rejects, and it would be `validate`'s first guest-content check.
- **`up` warns.** The handshake is part of readiness, so by then the features are
  honestly probed. Free, correctly sourced, early.
- **Attach fails hard**, naming both the rebuild and the repair verb. The facade
  is a general capability, so a machine that cannot be attached to is still a
  perfectly good machine; failing `up` over it is disproportionate.

**`vmlab machine capabilities` gains `attachable`**, meaning exactly **`tunnel`
and `fileops` are both present** — *this agent can serve an attach*, never *your
attach will succeed*. The narrow definition is load-bearing: identity is
declared separately and a flag promising success would become a lie. This is a
computed projection over probed facts (ADR-0004), not an inference from machine
kind. It deliberately does **not** widen to cover `watch`: workspace sync checks
`watch && fileops`, a different consumer with a different answer. A template
built with the agent disabled can neither be attached to nor host a workspace,
by construction, and surfaces through the same flag and the same error.

### 19.5 What the agent gains on the wire

Three vocabularies, deliberately unalike — which is the check that none was
cargo-culted from another. `tunnel` is a byte stream, `fileops` an out-of-order
RPC session, `watch` a single-request-at-a-time set swap.

**`tunnel`.** `OpenTunnel { id, host, port }`; the agent dials TCP inside the
guest; then bytes both ways. **Resolution is guest-side** — the host string
passes through verbatim, which is what makes a domain name in a SOCKS request
work. **No destination policy**: any address the guest can reach, not
loopback-only, since `-D` dials whatever the developer's tooling asks for and
vmlab is not a security boundary. A connect failure maps to
`SSH_OPEN_CONNECT_FAILED`, **not** `ADMINISTRATIVELY_PROHIBITED` — a SOCKS
client must distinguish "nothing is listening" from "vmlab refused you", and the
prohibited code is spent on things vmlab genuinely refuses. Only the facade ever
opens one; general host→guest TCP remains the Forward plan's job (§9.8).

**`fileops` is one channel that is an RPC session**, not a set of control
messages. Control frames are JSON and explicitly not flow-controlled, so a
40 MB editor-server push would arrive base64-inflated and sit in front of every
keystroke and metrics sample with nothing to throttle it; a channel per read is
worse, since OpenSSH's SFTP client keeps ~64 requests of 32 KiB in flight.
Instead `OpenFileOps` carries length-prefixed records inside the channel's own
credit window — JSON for metadata, raw bytes appended for a read or write
payload — which keeps agent-proto's "JSON for control, raw for bulk" split at
the record level and scopes handles to the channel so they die with it. Three
properties §19 fixes:

1. **Handle-based and offset-addressed** — `open → handle`, read/write at
   offset, `close`. A path-addressed vocabulary cannot express an SFTP client
   that opens once and writes 400 times, and cannot hold `O_APPEND` or
   `fsetstat` semantics at all.
2. **Pipelined: many requests outstanding per channel, replies matched by
   request id and free to complete out of order.** This is the throughput
   decision. Serialised against the measured 59–111 ms round trip, the facade
   would deliver under 1 MB/s where the raw channel does 80.
3. **SFTP-shaped by intent, in vmlab's spelling** — the facade *transcodes*
   rather than adapts, covering what `scp` and the editors issue
   (`open/close/read/write/stat/lstat/fstat/setstat/opendir/readdir/mkdir/rmdir/remove/rename/realpath`).
   Otherwise a tidier abstraction gets invented and discovers at implementation
   time that it cannot express `realpath` on a Windows drive letter. Two
   concrete instances of why transcoding rather than adapting is right:
   **`mkdir` carries a case-sensitivity flag** (§19.6 needs it, and NTFS only
   accepts it while the directory is empty, so it cannot be a later `setstat`),
   and **symlink creation carries the link kind**, because Windows requires
   file-vs-directory at creation and a dangling link does not reveal it.

**One file vocabulary, not two.** The whole-file, path-addressed
`OpenFilePush`/`OpenFilePull` pair and its `file` feature **retire**; `vmlab
cp`, the wscript push/pull, tree pushes and provisioning all move
onto `fileops`. Keeping both would hand every future consumer a choice, and the
syncer — the third consumer — needs `stat`, mtimes and offset writes, so it
would straddle them; it would also mean two guest-side write paths on each of
two targets and two sets of Windows path and ACL bugs. The migration is cheap
because tree pushes already walk the tree host-side. Two details stop it being a
silent regression: **`fileops` carries a `digest` request**, preserving the
guest-computed verification that today's transfer offers (and which the syncer
then leans on heavily), and **`mode` moves to `open`/`setstat`**, keeping "Unix
permission bits, ignored on Windows".

**`watch` reports paths, not events** — and that single move dissolves the
platform-divergence problem. The agent holds a **coalescing set of dirty paths**
that the host drains; a drained record is the path plus its **current stat**
(`kind`, `size`, `mtime`), or a tombstone if the path is gone — byte-identical
to the record the reconciliation stat-walk emits, so one vocabulary serves both
and **no platform event kind ever crosses the seam**. `inotify` and
`ReadDirectoryChangesW` disagreeing on renames, on in-place same-size writes and
on whether a directory delete implies its children therefore never becomes a
vocabulary problem. Per-event kinds are not a rejected option but an
*incoherent* one: a path created, modified and deleted inside one drain window
has no single kind, because coalescing has already destroyed it.

A pushed stream was rejected because its back-pressure converts directly into
`IN_Q_OVERFLOW` — under a `cargo build` the guest generates faster than the
credit window drains, the agent blocks, and its internal queue overflows. A set
turns an unbounded push into a bounded pull: a compiler writing one file 400
times is one entry. Latency is paid for by a single **`Dirty` nudge** on the
empty → non-empty transition, so the host drains immediately when idle and
batches naturally under load, and a build burst sends exactly one.

`OpenWatch { id, path, prune }` opens a **data channel** of length-prefixed JSON
records — `Dirty` (agent→host, unsolicited), `Drain` (host→agent, swapping the
set out atomically), `Batch` (the stat records) and `Rescan` (§19.6's overflow
value, replacing the batch). Three properties: a **data channel for
back-pressure, not tidiness** (a 30 000-path batch is megabytes of JSON, and on
the unflow-controlled control channel it would sit in front of every keystroke);
**no request id**, because at most one `Drain` is outstanding and a field that is
always the same value invites someone to build pipelining on it that set-swap
semantics cannot support; and **no batch ack**, because a dropped channel already
implies a stat-walk, so the loss self-heals through a path that has to exist
anyway. The watch root vanishing fails the channel **by name** rather than
degrading to `Rescan`, so the resulting halt can say *the workspace directory is
gone* instead of *the guest deleted 4 000 files*.

**Identity rides per-open and self-contained.** Every channel-opening message
that touches a user's resources carries an optional `logon { user, secret,
elevated }`; absent means the agent identity. The host resolves label → triple,
and the agent's cache stays a pure internal optimisation. A `logon_id` handshake
was rejected: it puts a resource's lifetime on the host, and after a snapshot
restore both sides discard channel state on the re-handshake, so the host would
hold a reference the agent has forgotten. With a self-contained open a
re-handshake costs nothing and §19.2's lifetime rules stay inside the agent. The
secret crossing repeatedly is a non-cost — it is a pipe on the same host, and
the secret is in the lab file in plaintext already.

It is carried by `OpenTerminal`, `OpenExec`, `OpenFileOps` and `OpenTail` —
§19.2's "everything a person invokes" — and **three opens never carry it**, each
for a stated reason so none reads as an oversight: `OpenTunnel` (a TCP connect
has no user context on either OS, and the field would imply a per-user network
view that does not exist), `OpenEventLog` (the Windows event log is
machine-scoped and its ACLs assume an administrator, so an ordinary login would
get a silently empty Security channel — a stated agent-identity read beats a
quiet empty one), and `OpenWatch` (a watcher *observes*; it produces none of the
developer's files, so §19.2's rule puts it on the agent identity — see §19.6 for
the reciprocal that creates).

One message is added in the guest→host direction and it is deliberately **not** a
channel open, so ADR-0013's invariant is untouched: **an agent→host `Eof`**.
The protocol has a host→guest EOF but no reverse one — the guest can only end a
stream by exiting or closing — and a TCP tunnel needs per-direction half-close,
or a peer that shuts down its write side tears the channel down and the absence
surfaces a year later as a hung tool nobody can reproduce. It incidentally gives
`exec` stdout a clean EOF.

### 19.6 The workspace

**The workspace is a guest-local working copy on the machine's own disk; the
host directory is canonical; a vmlab-integrated syncer keeps them in step**
(ADR-0014).

Neither obvious option survives. A host directory **shared in** cannot carry a
watched source tree at all: host-side edits do not reach `ReadDirectoryChangesW`
over virtiofs (no released virtio-win pushes the notification, and the merged
one polls every 3 s, watches only already-open handles and reports one
undifferentiated `MODIFIED`), and they do not reach a recursive
`SMB2_WATCH_TREE` over Samba (whose inotify backend registers a single
non-recursive watch). Both fail **silently** — the watcher stays armed and quiet
and the language server simply stops re-analysing. Linux guests are no better;
inotify does not fire for host-side virtiofs changes either, blocked at three
independent kernel layers. Beyond notification, virtiofs-on-Windows is a
self-described Tech Preview: case-**sensitive** by default, no alternate data
streams or hard links, a ~1023-character path cap against NTFS's 32 767, and
open tracker issues for `rm -rf` silently failing and for `cargo build` and
`git clone` breakage. Source **guest-side and on its own** fails the other way:
`destroy` is a first-class verb on disposable clones, and a snapshot restore
would roll uncommitted work back with the machine.

Three further mechanisms were designed and rejected, recorded because each is the
obvious next idea. A **lab-scoped workspace disk** — sound, and it would have
delivered rebuild-keeps-your-source, but once the host is canonical the guest
copy need not survive anything, so it lives in the clone; it survives only as a
possible *performance* option. A **vmlab-authored filesystem** (our own FUSE
client, WinFsp on Windows) — right about the mechanism and fatally asymmetric:
Windows works, Linux does not, because the kernel's FUSE notification path
raises no fsnotify events at all, so our daemon could invalidate the guest's
cache and still deliver no event. That buys one guest OS, colliding with *one
contract, every machine kind*, at the cost of §7.5's "largest single engineering
component" a second time, in driver-callback territory, twice. **Agent-injected
events on a share** — genuinely works as a mechanism, and still loses, because
it fixes one of the three findings above: the path cap, the case sensitivity and
the silently-failing `rm -rf` all stand, and *those* are the workload.

**With the host canonical, the snapshot footgun dissolves.** Restore rolls the
machine back and the workspace re-converges from the host; `destroy` takes the
clone and the guest working copy and loses nothing. That is `Rebuild Container`'s
promise — *resets everything except your local source* — delivered rather than
traded away. **Snapshots are not a workspace backup**, and §19 says so plainly.

#### Declaration

```
@dev(workspace = "./src", workspace_guest = "C:\\src")
```

The **host path is required** — the one fact only the lab author knows. The
**guest path is optional**, defaulting from the profile (`C:\src` on Windows,
`/src` on Linux). **At most one workspace per machine**; additional repos are
ordinary guest-local clones, unsynced. The argument must **not** be spelled bare
`guest`, and a `workspace {}` child block was rejected for the same reason: that
is `share {}`'s shape, a share is virtiofs/SMB passthrough where a workspace is a
synced local copy, and same-looking syntax for opposite mechanisms is how someone
puts their source in a `share {}` and loses their watchers. **`share {}` stays
exactly as useful as it was** for datasets, installers, artefact drops and
getting build output back to the host; the line is a *watched source tree*.

**Ignore rules are repo-tree-first and not a WCL argument**: a built-in floor,
then the repo's `.gitignore`, then `.vmlabignore` for the delta including
negations. `.gitignore` is the right default source because what you do not want
to sync is almost exactly what you do not commit — both are "reconstructible,
large, or machine-specific" — and `.vmlabignore` covers where *almost* fails,
since gitignored files you *do* want guest-side (`.env`, local certs,
`appsettings.Development.json`) need a `!` negation or the app will not start
and the reason will be invisible. They fail §19.1's third clause: they describe
the contents of a *directory*, so they travel with the repo, and they are
developer-owned — a stale `node_modules` rule must not require editing a
committed `vmlab.wcl` that every other developer shares.

An ignored path is not *skipped*, it is **guest-owned**. `node_modules` is the
proving case: you do not want it absent guest-side, you want the guest to run its
own install and hold guest-native binaries there, diverging permanently and on
purpose. Neither direction ever touches a guest-owned path, and guest-owned paths
are exactly the ones that do not survive a rebuild — correctly, because they are
reconstructible.

**`.git` syncs bidirectionally.** The deciding fact inverts the usual reasoning:
the guest can stay offline and for a domain lab usually will, so the **host** is
the side with network access and host-side `git fetch`/`pull` is a first-class
operation rather than an edge case. The contention decomposes: `.git` is mostly
immutable and additive — loose objects are content-addressed, packfiles and
indexes write-once — and that majority syncs freely because no two writers ever
produce different content at one path. The conflict surface is the small mutable
set (`index`, `HEAD`, `refs/`, `packed-refs`, `config`, `logs/`), and the rules
are: never sync `*.lock`; **defer** the mutable set while a lock is held on
either side; never auto-merge. That deferral is *timing*, not a conflict rule —
see below.

**The size guard refuses loudly, per file, before transfer.** It halts on the
offending path, names the file and the rule, and states the two ways out (add an
ignore rule, or raise the cap). Path rules catch what you know the name of; the
guard catches the 4 GB `.vhdx` nobody wrote a rule for, which is why it matters
most on first run in a repo nobody has set up. It fires *before* transfer
because the point is not to spend ten minutes pushing something unwanted, and it
is per-file so the failure never depends on unrelated files and the message can
always name a culprit. Shipping a safety net that failed as silently as the
transports this decision rejected would be a self-inflicted repeat.

#### The agreement point, and what a conflict is

**The developer authors guest-side.** *Canonical* is doing **durability** work,
not authorship work — the host copy is what survives `destroy`, not what anyone
types into. The host-side writer set is small and enumerable: git operations,
occasional host-side tooling, and vmlab's own restore re-seed. **A conflict is
therefore an anomaly, and §19 says so** — which licenses an expensive, loud, safe
policy instead of a winner rule that must be right thousands of times a day.
Treating the two sides as peers with a merge story was rejected: it buys
machinery for a workflow this design argued against, and makes the anomaly —
where work actually gets lost — quieter rather than louder.

The agreement point is a **host-side sync ledger**: one record per relative path
carrying a content digest plus **each side's own** `(size, mtime)` as a
change-detector. It lives in the lab's `.vmlab/`, per (machine, workspace), so
`destroy` wipes it — the guest tree is gone and there is nothing left to have
agreed with. Four properties:

- **Host-side only.** A guest-held copy is exactly the surviving guest-side state
  the workspace disk was retired to eliminate, and it can disagree with the
  host's.
- **Never compare a host mtime to a guest mtime.** Each side's mtime is compared
  only against its own recorded value. A restored guest resumes with a clock
  *behind* the host, so every file it holds would look older — which
  **disqualifies `newest-wins` outright**, before taste enters.
- **Digest is the truth; `(size, mtime)` is a pre-filter.** A same-size in-place
  write is exactly the case the share transports were caught missing.
- **A missing ledger is not a decision.** On first run, or with a wiped `.vmlab/`
  and a live guest, paths whose digests match are adopted as agreed for free and
  paths that differ take the ordinary conflict path. "No ledger means blind
  host→guest seed" is the version that eats a developer's work the one time they
  deleted `.vmlab/` to fix something else.

**Reconciliation is a guest stat-walk, not a Merkle tree.** The guest walks and
reports `(path, kind, size, mtime)`; the **host** applies the ignore set on
receipt and requests a `digest` only for suspects. A Merkle tree was designed and
rejected for a reason worth keeping: comparable subtree roots require the guest
to decide, **for a file it created itself**, whether that file is in the synced
set — and that decision *is* the ignore set. Every partial version leaves build
outputs in the guest's tree and out of the host's, so the roots never match.
Keeping ignore semantics **out of the guest** was worth more than O(depth) root
comparison. The steady state never uses the walk; it is the exception path —
first sync, ledger loss, overflow, post-restore re-converge.

Registration is a different act from filtering, and it *is* pruned. The host
computes a coarse **prune list** — ignored directory prefixes with no negation
reaching below them — and hands it to the agent at `OpenWatch`, which never
registers a watcher under them. What forces this is not volume but a resource
fact: `inotify` costs **one watch descriptor per directory** where
`ReadDirectoryChangesW` is a single recursive handle, `max_user_watches` defaults
to 8192, and a `node_modules` tree is routinely tens of thousands of directories
— so an unpruned registration is **silently incomplete** on Linux, the exact
failure class that disqualified the share transports. The distinction that keeps
the rule above intact: **the guest is never asked to decide, it is handed a
list.** The host still owns globs, negations and semantics entirely.

Per path, each side is `unchanged | modified | deleted | replaced-by-other-kind`
relative to the ledger. One side changed → propagate. Both changed → conflict,
with four riders: **both modified with identical content is not a conflict**
(adopt as agreed, transfer nothing — common after a host-side `git checkout`
lands bytes the guest already had); **modified one side / deleted the other is a
conflict, not delete-wins**, because deletion is unrecoverable and the
modification is not yet propagated; **mode-only changes are not conflicts** and
are not synced across kinds, since a bit that cannot be represented on one side
cannot be a disagreement; and **file↔directory replacement is a conflict**.

#### Windows costs vmlab three actions

Each is a precondition of the *mechanism* — a different category from "the
toolchain is the author's `provision {}`" — so vmlab does them rather than
documenting them.

1. **Set the NTFS case-sensitive flag on every directory the syncer creates, at
   creation.** The host (Linux) can hold `Foo.cs` and `foo.cs`; a default Windows
   guest cannot, and letting it happen means the second write silently lands on
   the first. The flag can **only** be set while a directory is empty, which the
   syncer's always is, and inheritance must not be relied on (Microsoft's own
   documentation contradicts itself on it). It also makes the shared
   `.git/config` correct rather than half-wrong: `.git` syncs bidirectionally, so
   `core.ignorecase` crosses the seam as one value for two filesystems, and a
   genuinely case-sensitive guest is what makes one shared value right on both
   sides. **NTFS only** — where the flag cannot be set, a case collision at that
   path becomes a loud refusal. Accepted price, in Microsoft's own words: some
   Win32 tooling that upper/lower-cases filenames breaks inside case-sensitive
   directories. No WCL knob for it — that is surface on speculation, and the
   failure is loud if it bites.
2. **Attempt symlinks, and warn on failure.** The guest being symlink-capable is
   a documented precondition (§19.4); vmlab does not work around it silently.
3. **Set `core.autocrlf = false` in the Windows guest's git config.** Git for
   Windows ships `true`, which would rewrite the whole working tree to CRLF on
   the first guest-side checkout, sync every file back as modified, and — if the
   host touched anything — halt the whole workspace.

**A Windows dev login declared `elevated = false` degrades the workspace in two
named ways: no case-sensitive directories and no symlinks.** Stated once here,
because otherwise both fail at a random path, hours in, looking like a vmlab bug.

#### Policy: halt and surface

**The whole workspace stops, both directions, on one machine.** The objection —
a halted syncer is a stopped dev machine — costs less than it looks, because
edits made during a halt are still one-sided for every file the host did not
touch, so they drain normally on resume rather than compounding.

- **Scope is one machine's workspace.** Not the lab, not other dev machines.
- **The watch keeps running.** Events accumulate; stopping it would guarantee a
  full rescan on resume. The host keeps draining into its own pending set while
  halted, so the guest set stays small and a long halt costs no rescan.
- **Finish the file in flight, then stop.** A torn half-written file is worse
  than one extra completed transfer.
- **Scan then halt, reporting every conflicting path in the batch.** A host-side
  `git pull` collides in *batches*; halting on the first would turn one `pull`
  into thirty resolve-and-resume round trips.
- **No automatic escalation.** Ten conflicts do not become a bigger hammer.

Rejected: **winner rules** (`newest-wins` is unsound above; host-wins destroys
the authoring surface and guest-wins destroys the canonical copy, both silently,
at scale, in a burst) and **conflict copies on disk**. The latter is worth
stating: **the two copies already exist, one per side**, and neither is written
or deleted by a halt — inventing `foo.cs.conflict-host` adds a file the build
sees, `git status` reports, and someone eventually commits.

**`.git` needs no special conflict rule.** A whole-workspace halt has no
granularity to argue about, so its carve-out shrinks back to what it always was —
never sync `*.lock`, defer the mutable set while a lock is held — which is a
**deferral**: a timing rule that clears itself, not a conflict rule.

**Resolution is host-side, necessarily**, and §19 states the reason so nobody
later "fixes" it: ADR-0013's invariant means there is no guest→host control path
at all, so a `vmlab` shim inside the guest could not call back even if one were
shipped. The seam-crossing worry is softer than it looks, because the host copy
is a plain directory on the developer's own workstation — inspecting it is `cd`,
not a remote operation. Only the *guest* copy is behind the seam, which is why a
`diff` that pulls the guest version host-side earns its place. Resolution routes
are per-path `--host` / `--guest`, `--all` for the batch, and a free third route
needing no verb at all: make both sides identical by hand and the next pass
adopts them as agreed.

**The guest-side signal is a marker file at the workspace root**, listing the
halted paths, in the built-in ignore floor so it never syncs. From inside the
guest a halt is otherwise *nothing happening* — the file simply stops updating —
which is the silent-divergence failure this section keeps ruling out, on the one
side no control path can reach. An SSH banner was rejected as the primary signal
because it fires only on a *new* attach, useless to everyone already working when
the halt happened; it survives as a secondary. The marker file's `git status`
noise is a feature: it is the developer noticing.

#### Timing, deletions, durability

**Per-path debounce, both directions** — not a performance tweak: without it the
syncer reads files mid-write (editors write-temp-then-rename, compilers write in
chunks) and a partial read guest→host writes a torn version over the canonical
copy.

**Volume warns and continues; it never halts.** The distinction: **the size guard
refuses because a 4 GB `.vhdx` is unwanted work; a build burst is wanted work
that happens to be large.** Halting on volume would let a `cargo build` into an
un-ignored `target/` stop the dev machine. The warning names the path and
suggests a `.vmlabignore` rule. Bursts **de-prioritise, never drop** — a burst
under one subtree must not starve a single save elsewhere, which is the
difference between "slow" and "broken".

**Overflow warns, forces a rescan, and never halts.** Both platforms lose events
whole-tree rather than locally (`IN_Q_OVERFLOW` is queue-wide; a recursive
`ReadDirectoryChangesW` handle signals overflow with a zero-length return), and
the agent's dirty set must be **capped** or a micro-VM's set is an unbounded
allocation. All three sources collapse to a single `Rescan` value replacing the
batch, and the host runs the stat-walk; it never needs to know which fired. The
cap doubles as the batch bound, so a drain never needs pagination. **The rescan
is a barrier in both directions, not a background task** — this is the
non-obvious half. Between the overflow and the completed walk the host does not
know the guest moved; if it kept propagating host→guest it would see "host
changed, guest unchanged" and overwrite guest work silently, through the ledger,
with no conflict ever raised. That is a **deferral**, not a halt: no developer
action, no resolution.

**The guards on deletion are asymmetric on purpose, because the two sides are not
equally valuable** — the guest is reconstructible and the host is not.
Host→guest deletes are unguarded; a `git checkout` removing 400 files just
removes them. **Guest→host bulk deletes halt** past a threshold expressed as a
proportion with a floor (a fixed count punishes large repos; a bare proportion
lets a ten-file project lose everything). This exists not for deliberate deletion
but for the guest doing something catastrophic and the syncer faithfully
replicating it onto the canonical copy. A single deletion still propagates
immediately; the guard is about mass.

**Renames are delete + create at the ledger level.** The two platforms disagree
on rename events, so a syncer that *depends* on one forks by platform, which
*one mechanism, every machine kind* forbids. Delete+create needs no rename event
at all. The cost — retransferring a renamed 200 MB file — is recoverable by
digest, where a delete/create pair matching content the other side already holds
may be satisfied with a local rename: **permitted, not required**. **A directory
delete expands via the ledger**, not the event stream: the platforms disagree on
whether children are reported, the ledger knows exactly what was agreed to be
there, and anything else in that directory was guest-created and unsynced anyway.

**Every apply is temp-name-then-rename**, both directions, with the temp in the
**same directory** as its target — so the rename is atomic rather than a
cross-volume copy, and on Windows it inherits the case-sensitivity flag set at
`mkdir` — and in the ignore floor so it never becomes a sync object itself.
**The ledger records agreement only after the rename**, never after the last
write: otherwise a crash between the two leaves the ledger claiming agreement on
a file that was never placed, the next pass concludes "unchanged", and the
divergence is permanent and silent. **Resume is re-transfer, not offset-resume** —
the source may have changed while the channel was down and there is no cheap way
to know which prefix is still valid.

**Symlinks sync verbatim and are never followed.** Never-follow is the
load-bearing half: a link pointing at `/` that the syncer follows walks the
entire host filesystem into the guest. The target string is content, recorded in
the ledger like any other, and vmlab does **not** translate targets across the
seam — a link to `/usr/lib/foo` lands verbatim on Windows and dangles, which is
correct under §19.8's line that vmlab moves bytes it is told to move and never
interprets them. **Special files are skipped loudly** — FIFOs, sockets, device
nodes, and Windows reparse points that are not symlinks. A build leaving a
`.sock` in the tree is normal and must not stop a dev machine; omitting it
silently is the failure mode this section keeps rejecting. They never enter the
ledger, so they cannot produce a phantom conflict.

**The reciprocal of `OpenWatch` running as the agent identity** (§19.5): the
watch is a *superset* of what the login can read, so a drained path the login
cannot open fails at digest-or-read time. That is a **loud, named skip**, the
same treatment special files get, and explicitly **not** a halt — a build leaving
a root-owned artefact in the tree must not stop the dev machine. The alternative
is worse: as the login, a directory the login cannot traverse would be *silently
unwatched*, a subtree that quietly stops syncing.

#### Snapshots, ignore changes, two machines

**Restore brackets the syncer.** A restore rewinds the guest by 500 files at
once, which a naive bidirectional syncer cannot distinguish from *the developer
having edited 500 files* — and it would propagate them to the host, overwriting
canonical work with old versions, silently. **vmlab performs the restore**, so it
can bracket: guest state for that tree is discarded and the tree re-converges
host→guest. This is the argument that the syncer must be **vmlab-integrated**
rather than a generic tool wrapped; an off-the-shelf syncer cannot know a rewind
happened.

Re-convergence is a **host-only, digest-based reconcile**. The guarantee is
directional — nothing flows guest→host, so old guest state cannot come back. The
guest tree is walked for the sole purpose of deciding what to overwrite and
delete: overwrite anything differing from host truth, delete anything the ledger
does not hold, transfer nothing else. The guest is **inspected, never believed**;
it contributes nothing to the ledger. **It must compare by digest**, stated
rather than left implicit because the cheap mtime version looks correct and
silently keeps exactly the state the reconcile exists to destroy — a restored
guest's clock runs behind the host, so a same-size in-place write compares
identical on `(size, mtime)`. A literal wipe-and-re-transfer trivially satisfies
the guarantee and remains legal; it is not required. **The re-seed completes
before the watch re-opens**, or the syncer's own writes fill the fresh dirty set
with tens of thousands of self-inflicted paths.

**A pre-flight flush brackets capture as well as restore.** If the guest has
unsynced work — channel down, sync paused, guest writing faster than it drains —
**halt and surface**. Flushing before capture also makes the snapshot coherent
with the host tree, so restoring it lands somewhere meaningful rather than
mid-transfer. `snapshot restore` needs an **explicit discard flag**, because
restore discards the guest side by design and would silently destroy the guest
copy of every conflicted path — but refusing outright is obstruction, since
wanting to throw the guest away is frequently *why* you restore. `capture`
refuses with no escape.

**There is no resync token, and it costs nothing**, because the list of
stat-walk triggers — first sync, ledger loss, overflow, post-restore
re-converge — **is exactly the list of watch discontinuities**. There is no fifth
case, so a token would be surface with no consumer. Two routes re-establish
agreement rather than one: an agent restart or channel blip takes the
**stat-walk** (the guest kept running and may have changed underneath us), while
a snapshot restore takes the **bracket's re-seed with no walk** (vmlab already
knows what is in that tree).

**When the ignore rules change** — they live *in the tree* and are
developer-owned, so they change under the syncer. **Leaving scope is free**: a
newly guest-owned path leaves the ledger, both copies stay, neither side is
touched again. **Entering scope is a conflict**: no agreement point exists and
both sides may hold content, so un-ignoring a populated directory halts with
every file in it named. That is correct for the case that actually matters — the
files most likely to be un-ignored are `.env`, local certs and
`appsettings.Development.json`, where the two sides differing is the *normal*
situation and picking a winner silently overwrites a working local config with a
stale one. The 30 000-file version is rarer, self-inflicted, and one `--all`
away. **The ignore rules' own digest is part of the ledger**, so the halt can say
*these conflict because you just changed the rules*. This is also what keeps the
prune list correct across a rules change, for free: entering scope is already a
halt, which forces a rescan, so re-registration rides an event that exists.

**Two dev machines may share one host workspace** — same source, built on a
Windows member and a Linux container side by side. Allowed because of the
topology rather than by luck: **the host is a hub, not a peer.** Each machine has
its own ledger against the host, so there is never a guest↔guest comparison; an
edit on A lands host-side and B then sees the ordinary one-side-changed case. The
halt message **names the machine** whose push it is conflicting with, and there
is **one halt per machine** — A halting on its own divergence must not stop B.

**Guest-side git is a target workflow, not a tolerated edge case.** A coding
agent working in the dev machine commits, branches and diffs constantly and has
no host shell to do it from, so `.git` is a hot bidirectional path and the lock
deferral is load-bearing rather than defensive. Running git on both sides at once
remains a documented way to reach a halt, which behaves correctly there — both
copies survive. **Line-ending policy belongs in each side's *global* git config,
never the repo's**, because the home directory is guest-local and `.git/config`
is shared: the thing that must differ per side goes in the one place that is not
synced. `.gitattributes` is the documented escape for genuine CRLF needs. **The
syncer translates nothing** — bytes cross verbatim and git does all normalisation
on both sides, from settings that now agree.

### 19.7 The verb surface and the host-side footprint

**The facade is general, so its verbs are top-level; `vmlab dev` holds only what
is meaningless for a machine that is not `@dev`.** That rule — *a verb earns the
`dev` noun only if it is meaningless for an unmarked machine* — decides the
whole surface, and it matches how §12's table already sorts: the reach-into-a-
guest verbs are top level while `vmlab machine` carries only capabilities and
stats.

**Top level, any machine:**

| Verb | Behaviour |
|---|---|
| `vmlab ssh <machine> [-- cmd]` | Refreshes the managed SSH block, then **`exec`s the system `ssh`** against the alias. Not a second SSH client: one implementation of the client side, and it is the one editors already use. Takes a bare name in a lab directory or `<lab>/<machine>` from anywhere. |
| `vmlab ssh-proxy <lab>/<machine>` | **Hidden** — not in `--help`, not in §12. The `ProxyCommand` target, never typed by a human. |
| `vmlab ssh-config [--print <machine>]` | Refreshes the managed block. `--print` emits the stanza plus the editor settings snippet (§19.8) for a client that will not read the file. |

**Under `vmlab dev`:**

| Verb | Why it earns the noun |
|---|---|
| `vmlab dev attach [machine]` | Cold-to-editing in one command: ups, waits for `attachable`, becomes a shell. |
| `vmlab dev use <machine>` | Records which dev machine is *mine* — host-side, because `vmlab.wcl` is committed and structurally cannot say it. |
| `vmlab dev sync status \| flush \| diff \| resolve` | The workspace exists only for a dev machine. `status` carries the halted-path list, volume warnings, overflow/rescan symptoms and loudly-skipped special files; `resolve` takes per-path `--host`/`--guest` or `--all`; `diff` pulls the guest copy host-side; `flush` is nearly free, since bracketing snapshot capture and restore already requires the machinery. |

**Two verbs deliberately not added.** There is **no `dev list` or `dev status`**:
lab status is a typed projection (ADR-0004) and a dev machine is a machine, so
the projection **widens** to carry `dev` and `attachable` and `vmlab status`
shows them, rather than standing up a second status verb reporting on a subset of
machines. And there is **no `rebuild` verb** in either spelling: `vmlab vm
destroy <m>` (or `vmlab container destroy`) followed by `vmlab up <m>` already
*is* re-clone plus re-provision, and §19.6 means the workspace survives it. An
alias's only job would be to hide which of three operations it performed, and
that hiding is actively dangerous — a domain member rotates its computer-account
password roughly monthly, so a `rebuild` that quietly chose snapshot-revert would
hand back a machine that boots and cannot authenticate. **§19 states the
equivalence — *`Rebuild Container` is `destroy` + `up`, and your workspace
survives it* — and adds no verb.**

**`vmlab dev attach` launches no editor and knows none.** It ups the machine,
waits, and **becomes a shell on it**, printing the alias and the editor snippet
alongside; the developer opens their own editor and picks the alias out of the
picker, which the managed block guarantees is there. A host-config `editor`
command template mirroring the existing `viewer` key was real prior art and was
rejected on coupling: vmlab learning an editor is exactly what §19.1 rejected
editor hints for. **Consequence, because `attach` becomes a shell: the workspace
syncer must not be tied to that process's lifetime.** Closing the shell cannot
stop sync while the editor is still attached — the syncer is lab-daemon-owned and
`attach` starts nothing it owns.

**Lifecycle differs by caller, and one of the three is forced rather than
chosen.** `vmlab ssh-proxy` **never** does lifecycle: it is spawned by the editor
with no TTY, its stderr may never be shown, and clients spawn several
concurrently — so "boot and wait" becomes a silent multi-minute hang that races
itself, and the client's own connect timeout kills it long before a
domain-joined Windows guest finishes booting. It fails immediately with a
diagnostic that survives being printed into an editor log. `vmlab ssh`
**refuses** if the machine is down and reports why, matching `console` and `exec`,
which do not secretly start machines either. `vmlab dev attach` **ups and
waits**, because cold-to-editing is its entire reason to exist and progress goes
to a terminal it owns.

**The host-side footprint is one artefact: a marker-fenced block vmlab owns
inside `~/.ssh/config` itself.** A vmlab-owned file plus an `Include` was the
obvious shape and it fails on evidence: **JetBrains Toolbox's config importer
does not follow `Include`** (proven against a four-stanza control — `Include`d
hosts never appear in the picker, with or without a `ProxyCommand`, even with the
`Include` at the very top), while VS Code resolves `Include` in its own parser
and `vmlab ssh` never needed it either. So the `Include`'s entire value was
serving third-party clients, and the one client that needs it cannot read it. The
tempting half-measure — keep a private file and reach it with `-F` — is rejected
*because* it works: vmlab's own commands would keep succeeding while every editor
saw nothing. **Sharing one path means a broken or displaced block breaks `vmlab
ssh` too**, deliberately, so the developer meets the failure at a terminal that
can explain it. It also leaves the developer's own `Host *` settings applying to
vmlab connections, which `-F` would silently discard.

The block is **deterministically ordered** (lab → machine → login label), so a
dotfiles-tracked config shows a diff only when something really changed; it
**prunes itself by lab root**, since each lab's stanzas carry that lab's
canonical root in a machine-readable comment and a root that no longer holds a
`vmlab.wcl` has its stanzas dropped — the block *is* the record, and no
bookkeeping file exists; and it **refuses to write on mangled markers** (one of a
pair, duplicated pairs, `END` before `BEGIN`) with an error naming file and line,
because vmlab does not attempt repair on a file it does not own.

**Stanzas cover *declared* machines, not running ones.** An alias means "this
machine exists in this lab", not "it is attachable right now" — liveness is
`vmlab status`'s job and the refusal path above. Listing only running machines
would empty the editor's picker at exactly the moment you want it. The block
therefore **accumulates**, written from the `vmlab.wcl` the CLI already has in
hand: **any command that successfully loads a lab** renders the block and
compares it to disk, writing only on a real difference, so working inside a lab
directory is enough to register it. A failed write **warns**, except at
`vmlab ssh` and `vmlab dev attach` where the alias is load-bearing and the
command **fails hard with the reason** — the same ladder §19.4 sets for agent
capability. Mechanics that follow, each a way to lose someone's file: an advisory
`flock` across read-modify-write; a temp file in the same directory, fsynced and
renamed onto the **resolved** path, so a stow/chezmoi symlink keeps its symlink;
an absent file created `0600` under a `0700` `~/.ssh`.

**Placement stops being a parsing problem.** OpenSSH takes the first value it
obtains for each keyword, so an earlier `Host *` setting `ProxyCommand` or
`ControlPath` silently wins. Every write therefore **re-hoists vmlab's own region
to the top** — relocating its own region and never moving a line the developer
wrote — and then runs **`ssh -G <alias>`** and checks the resolved `proxycommand`
is vmlab's, erroring loudly and naming the keyword and pattern that beat it if it
is not. That is OpenSSH's own resolver, the one every client shells out to, and
it catches displacement, an overriding `Host *`, a stale hand-paste and a
redirected block alike with one mechanism and no ssh_config grammar in vmlab. The
escape is **a host-config path override naming the file vmlab manages its block
in** — a *location* knob with one code path behind it, not an on/off with two;
the `ssh -G` check still runs against it, so a redirected block warns honestly
rather than pretending to work. It doubles as the seam that makes the writer
testable without a real home directory, which matters for a component whose
failure mode is "ate someone's ssh config".

**Alias shape is `vmlab-<lab>-<machine>`**, with `vmlab-<lab>-<machine>-<label>`
for each non-default login (§19.2) — so "attach as admin" is a pick in the
editor's host list rather than something you have to know to type, and it is the
only way elevation is reachable from an editor that invokes `ssh <alias>` and
nothing else. It is typeable, tab-completable, and the prefix namespaces the
block against the user's own aliases. `<lab>/<machine>` is **disqualified as an
alias** because it lands in `ControlPath` via `%n` and a slash turns the mux
socket path into a nonexistent subdirectory; it survives as the *argument* form,
which is what `ssh-proxy` takes. Host-global uniqueness is ADR-0011's.

**`ControlPath` is `$XDG_RUNTIME_DIR/vmlab/ssh/%C`.** The real budget is **90
bytes, not 108**: `muxserver_listen` binds a temporary `"<path>.<16 random
chars>"` *before* the `sun_path` length check, so 108 − 1 − 17 = 90 usable bytes.
`%C` is OpenSSH's own token — 40 hex characters, **bounded by construction** —
and under the runtime directory that is ~66 of the 90 on any home directory and
any uid. Two things come free: the mux socket moves out of the config directory
into the runtime directory where every other vmlab control socket already lives,
and vmlab invents no naming scheme it would then have to keep stable. The stanza
sets no `HostName`, so `%C` varies per alias. Stating a length limit and refusing
at generation was rejected as the wrong direction of coupling — a lab would be
valid on one machine and invalid on another because of how long the developer's
home directory is — leaving the durable rule:

> **Anything vmlab puts in a Unix socket path is bounded by construction, never
> by a name it does not control.**

**Host keys and withdrawal.** The guest holds no host key at all, so a template
clone cannot carry a stale one and a snapshot restore cannot roll one back. What
remains: the key is per (lab, machine) and **survives `destroy`**, so destroying
and recreating `dev01` presents the same key and the `known_hosts` entry never
needs rewriting. A recreated machine inheriting a name's identity costs nothing,
since the real trust boundary is reaching the lab socket. `destroy` withdraws the
master with **`ssh -O exit <alias>` before removing the stanza** — the tool's own
way to kill a multiplexer, and it needs the stanza to still resolve — then
removes the stanza and the host key stays.

**Which machine is mine**, resolved when a `dev` verb needs a machine and none was
named: an explicit argument → `VMLAB_DEV_MACHINE` → the `vmlab dev use`
selection → `@dev(default = true)` → a lone `@dev` machine → otherwise **error,
listing the candidates**. Never guess. The selection is stored in the lab's own
**`.vmlab/`**, which §4 already says should be gitignored — that makes it
per-developer by construction, which is exactly what a committed `vmlab.wcl`
cannot express, and it needs no key at all since it lives inside the lab it
describes. `destroy` clears `.vmlab/` and therefore forgets the selection;
re-setting it is one command.

**What this costs the wire (ADR-0007): three new lab commands.** `vmlab ssh`,
`ssh-config`, `dev use` and `dev attach` add nothing — they generate client-side
or compose existing commands. `vmlab ssh-proxy` is **one** new command, the
proxy's channel to the machine's agent (§19.3), and the syncer's `status`/`flush`
are **two**. Dev-ness in `vmlab status` is a projection widening, not a command.

**No other surface joins the SSH path.** The facade's whole point is a stdio
pipe for a *local* editor, so no vmlab surface beyond `ssh-proxy` offers an SSH
affordance of its own. (The browser console this paragraph once excused was
removed before release; the reasoning stands without it.) **Sync-conflict
resolution is CLI-only.** So `ssh-proxy` is a deliberate one-way command
carrying a genuine reason rather than a gap. Note also that
today's `vmlab shell` runs as the agent identity, so `vmlab ssh` is not a
duplicate path — it is a **different identity to the same guest**.

### 19.8 Editors, extensions, and the offline guest

**The client set, stated as a matrix rather than implied.** vmlab publishes SSH
and nothing else, so any SSH-capable client attaches — but for a *Windows* dev
machine the set that actually works is narrower than it looks:

| Client | Linux dev machine | Windows dev machine |
|---|---|---|
| plain `ssh` / `scp` / `sftp` | yes | yes |
| VS Code Remote-SSH | yes | yes |
| JetBrains Toolbox App | yes | **no** — its deploy bootstrap is `/bin/sh -c …`, a POSIX shell reading a script off stdin (observed) |
| JetBrains Gateway | yes | no — Linux backends only, documented |
| Zed | yes | no — no Windows remote server |

So **JetBrains remote dev serves a Linux dev machine**, and the Windows dev
machine is served by VS Code Remote-SSH and plain `ssh`. §19 says that plainly
rather than implying Rider covers Windows.

**The guest can stay offline.** VS Code's `"remote.SSH.localServerDownload":
"always"` makes the *client* download the 12 MB server and push it over `scp`,
observed end to end through the facade. It is a **client-side** setting, so a dev
machine cannot make itself offline-capable unilaterally — which is why
`vmlab ssh-config --print` hands the developer a settings snippet beside the
stanza, carrying that setting and `remote.SSH.remotePlatform: windows` (the
documented workaround for a Windows host-detection bug). Pre-staging the server
into a template works mechanically but is keyed to the *client's* build commit,
so it dies on every editor update; the push route is version-agnostic by
construction.

**Extensions are toolchain, and toolchain is a declaration. vmlab builds
nothing.** Worked through for two structurally opposite editors — VS Code
(client/server split, marketplace fetch) and Neovim (a TUI over the facade's own
`session` channel, plugins that are `git clone`s) — they converge on one finding
that dissolves the question: **the blocker is never the editor, it is that
everything editor-shaped lives in a per-user home directory.** A template is
lab-independent, so a *domain* user's profile cannot exist at build time; it is
created on first logon by `LoadUserProfileW` (§19.2).

**The durable home is the *declaration*.** Extensions and plugins live in the
guest home, outside the workspace, so they survive reboot, `down`/`up`, and
restore to a snapshot taken after install; they die on a per-machine `destroy` + `up`
and on restore to a snapshot from before install. Both declared placements
re-apply across a rebuild — a template build because the clone is re-made from
it, a lab `provision {}` because a fresh clone boots first-boot again. Only
hand-install does not:

> **Bake what the lab needs every developer to have; hand-install what you
> personally want today, and expect to redo it after a rebuild.**

That closes devcontainers' `customizations.vscode.extensions` gap without §19.1's
rejected editor hints. A **per-machine durable home overlay** was rejected: it
reintroduces exactly the surviving guest-side state §19.6 retired the workspace
disk to eliminate, and for *less* reason, since unlike source, editor bits have a
canonical durable home already.

**One guarantee §19 states, because nobody would infer it:**

> A `provision {}` step can address the dev login's home directory **before that
> user has ever logged on.**

§19.2's precedence makes it true — a provision script passes `user:` to `exec`,
vmlab mints that logon, `LoadUserProfileW` creates the profile, and the write
lands in the real dev user's home. It is written down because §19.2's headline is
"everything vmlab does on its own behalf keeps the agent identity", which makes
"provision runs as SYSTEM, full stop" the natural implementation — and then the
Windows domain example silently fails. There is nothing to check statically, so
it adds no §5.1 rule.

**`provision {}`, never `playbook {}`.** A playbook runs config-weave in-guest
with no user parameter and has no rung on the precedence ladder. That is not a
total block, which is what makes it dangerous: the agent identity *can* write
into a profile directory that already exists, but cannot create one or set
ownership — so a playbook half-works on an existing profile and fails on a fresh
domain user, which is the first-run case. The rule generalises past editors:

> **Anything that must land as the developer rather than as the machine belongs
> in `provision {}`.**

Which is the same rule §19.2 drew when it carved out the workspace syncer.
Giving `playbook {}` a `login` field was rejected as surface added on
speculation — neither worked example needs it, and it is an ordinary later
feature request rather than a §19 decision.

**Personal config is the developer's and needs no decision.** A `dotfiles`
argument is already dead by §19.1's third clause, and devcontainers itself puts
dotfiles in a *client* setting rather than in `devcontainer.json`. The facade
already answers it with nothing built:

```
scp -r ~/.config/nvim vmlab-probe-dev01:.config/nvim
```

§19 prints that line, because a developer who does not know SFTP is available
over the alias will assume the offline guest has cut them off.

**Reaching a host-side service from an offline guest** — a package mirror, a
proxy, a licence server — is the other thing that looks like a dead end and is
not. `ssh -R` is refused (§19.3), but the answer needs no reverse tunnel: give
the dev machine a NIC on a segment with egress, and the NAT engine terminates
guest flows in-process and proxies them over ordinary host sockets, so anything
addressed to the gateway but off-segment reaches the host's own address. §19 says
so for the same reason it prints the `scp` line.

**Two worked examples, split by machine kind** so *one contract, every machine
kind* is shown rather than asserted: **VS Code on a Windows domain member** —
client/server, `%USERPROFILE%`, a minted domain logon, the case §19 exists for —
and **Neovim on a Linux container micro-VM** — no server, `~/.local/share/nvim`,
the container identity floor. Both use the *proven* placement (a lab
`provision {}` addressing the dev login's home), with baking into
`C:\Users\Default` named as the option for shipping a template to a team. The
bytes arrive by `media {}` (§6.3, whose stated primary use is already payload
delivery to guests with no network) or from the repo.

The line that keeps all of this from sliding into a feature:

> **vmlab moves bytes it is told to move and never interprets them.** `media {}`
> does not know a VSIX from a driver bundle; `provision {}` does not know
> `code --install-extension` from `winget install`.

### 19.9 Non-goals

- **A guest-initiated channel open, and with it `ssh -R`.** ADR-0013's invariant
  refuses it. The stated need is already met by NAT egress (§19.8), and no named
  client wants one: VS Code's real forwarding is dynamic and local, and every SSH
  and gateway jar in JetBrains Toolbox has zero reverse-forwarding strings. True
  `-R` would be a separate agent-proto effort — one message mirroring the host's
  open, plus guest-side listener lifetime — and the invariant is what keeps that
  a visible amendment rather than a drift.
- **Remote or multi-host development.** Attaching from another machine over the
  network is §1.2's single-host non-goal.
- **Being a security boundary** between the developer and the guest (§1.2).
- **Shipping or bundling editor servers or licensed components.** vmlab publishes
  SSH; the editor brings its own backend.
- **vmlab placing extensions itself.** It would require learning an editor's
  on-disk layout, which is the attach contract's stated non-goal.
- **Consuming `devcontainer.json`.** Deriving a lab from an existing devcontainer
  definition is a separate effort in the other direction.
- **An editor plugin presenting sync state.** §19.6 leaves it somewhere to attach
  — the marker file and `dev sync status` — and building it is a separate effort.
- **A `rebuild` verb and a host-config `editor` launcher**, both for the reasons
  in §19.7.
