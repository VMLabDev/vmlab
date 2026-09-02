# Templates, the store, and registry distribution

## What a template is

A template is a sealed, read-only disk image that VMs boot linked clones of.
Build it once from an installer ISO or an existing disk, store it under a name
and version, clone it into any number of labs.

A store entry is two files: `disk.qcow2`, the sealed image, and `template.wcl`,
its metadata. The metadata records the hardware the template was built with
(profile, CPUs, memory, disk size, firmware, TPM, secure boot, display), where
it came from, the version of `vmlab-agent` baked into it, the wscript surface
version its embedded scripts were written against, an optional first-boot
script, and the OCI repository it publishes to. The hardware fields form the
template layer of the resolution chain (VM > template > profile): a VM that
does not set `memory` inherits it from here. See lab-file.md and vm.md.

Templates are keyed by **arch, name and version**. A reference is written
`<arch>/<name>[@<version>]`. The arch is mandatory and never inferred from the
host, because it selects which QEMU system emulator boots the image. Omitting
the version means the highest in the store. `vmlab validate` rejects an
archless reference.

## The store

The store lives at `~/.local/share/vmlab/templates/`, laid out as
`<arch>/<name>/<version>/`. Reads are lock-free. Every mutation — install,
removal, import — holds an exclusive lock on the store, and content only ever
enters it by an atomic rename of a fully staged directory. A build or import
that fails part way leaves nothing behind.

The supervisor owns the store. Every `vmlab template` verb is a protocol
client: the CLI reads its own surroundings, such as a relative path or the git
remote of the current directory, and sends them to the supervisor, which is the
only process that opens the store or dials a registry. Consequences: a build
started in one terminal can be listed and stopped from another with `vmlab
template stop`; and a build needs a supervisor, which the CLI starts for you.

Verbs (flags in cli-template.md):

- `vmlab template list` — show what is installed.
- `vmlab template rm` — remove one version; refuses without `--force` when that
  disk still backs a linked clone in some lab.
- `vmlab template clean` — prune superseded builds per family, keeping the
  newest by default; a dry run until you pass `--yes`.
- `vmlab template build` — build a `template {}` block.
- `vmlab template stop` — stop an in-flight build or push.
- `vmlab template export` / `import` — portable `.tar.zst` archive, offline path.
- `vmlab template push` / `pull` / `search` / `login` / `registry` — registries.

## Building a template

A template is declared in a `template` block, in its own WCL file or beside a
`lab`, and built with `vmlab template build`. The block names the build's
**source**, the **hardware** the build VM boots with, any **media** to attach,
and the **provision** scripts and playbooks that drive the install.

```wcl
// examples/templates/ubuntu-24.04/vmlab.wcl
// Ubuntu Server 24.04 template (PRD §6.1). The installer ISO is downloaded
// and sha256-verified into the artefact cache; cloudinit/ is packed into a
// CIDATA ISO that subiquity picks up as a NoCloud autoinstall source. The
// provision script answers the autoinstall confirmation and waits for the
// installer to power the VM off. Build with:
//
//   vmlab template build        (run from this directory)

import <vmlab.wcl>

template "ubuntu-24.04" {
  arch    = "x86_64"
  version = "24.04.4"
  profile = "linux-modern"
  cpus    = 2
  memory  = 4GiB
  disk    = 20GiB

  source "iso" {
    url    = "https://releases.ubuntu.com/24.04/ubuntu-24.04.4-live-server-amd64.iso"
    sha256 = "e907d92eeec9df64163a7e454cbc8d7755e8ddc7ed42f99dbc80c40f1a138433"
  }

  media { kind = "iso" from = "./cloudinit/" label = "CIDATA" }
  nic { nat = true }

  provision "scripts/install.ws" { }
}
```

The build runs as a synthetic one-VM lab: a `scratch` VM whose primary disk is
pre-seeded from the source, on the hardware the block declares over the source
template's recorded hardware. It reuses the whole lab runtime, so a build script
has the same API a lab provision has. The flow: create the working qcow2, boot
it, run the provision steps, wait for the guest to shut down, verify the agent's
handshake, then move the disk and its metadata into the store under the new
version. Build output streams to your terminal as it happens.

A failed build leaves nothing in the store. The build's working directory is
removed on success and on failure, and the supervisor sweeps any build directory
a killed process left behind the next time it starts. A supervisor restart fails
every build in flight; there is no resumption.

### The agent bootstrap ISO

vmlab attaches one extra ISO to every build VM, labelled `VMLAB`. It carries the
agent binaries and an install script per OS. The template's own
unattended-install hook — a cloud-init `runcmd`, a subiquity late command, an
autounattend first-logon command — mounts that ISO and runs the script. The
guest installs the agent itself, so no host channel is needed before the agent
exists, and the build verifies the handshake before sealing. Clones never see
this ISO. Set `agent = false` on the block to skip baking the agent, at the cost
of a template that never reports ready.

### Versions

The block's `version` is a fixed prefix identifying the upstream release. vmlab
appends a build counter, so building the Ubuntu example once produces `24.04.4.0`
and again `24.04.4.1`. The counter continues from the highest tag already
published when the block names a `registry`, and from the local store otherwise.
Change the prefix and the counter restarts at zero. Pass `--version` to pin a
version instead; a version already in the store is refused.

## Media built from folders

A `media` block turns a folder on disk into an ISO or a floppy image and
attaches it to the build VM, or to a lab VM. This is how unattend files, driver
bundles and payloads reach a guest that has no network. Built images land in the
lab's `.vmlab/media` and are content-addressed by a digest over the folder's
contents, the kind and the label, so an unchanged folder never rebuilds.

ISOs are built with `xorriso`, falling back to `genisoimage` and then `mkisofs`,
with Joliet and Rock Ridge extensions so both Windows unattend media and Linux
payloads read correctly. Floppies are 1.44 MB FAT12 images built with `mformat`
and `mcopy` from mtools. A `disk` block with `from` does the same for a larger
FAT disk. See shares-media.md.

## Linked clones and scratch VMs

`vmlab up` creates each VM's disk as a qcow2 overlay whose backing file is the
template's disk in the store. The template is never written to. Clones live in
`.vmlab/`, survive `down`, and are deleted by `destroy`, after which the next
`up` makes fresh ones. Clones grow as the guest writes, which is why the
supervisor watches free space on the filesystems holding `.vmlab/` and the store
and emits `host.disk_low`.

`template = "scratch"` is a reserved name meaning no backing image. The VM gets
a freshly created blank qcow2 instead of a clone, and its hardware chain
collapses to VM block then profile. Validation therefore requires an explicit
`arch`, `profile` and `disk`. Boot media is yours to supply, typically a `cdrom`
or a `media` block. `scratch` never appears in the store and cannot be pushed or
pulled. It is for installer development and OS builds, where starting with no OS
is the point.

## Moving a template between machines

Two paths carry a template off the host it was built on. Both preserve the
metadata, so a moved template inherits into VMs exactly as it did at home.

- **Export and import.** `vmlab template export` writes one portable `.tar.zst`
  archive holding the disk and its metadata; `import` installs it into another
  store. This is the offline path.
- **Registries.** `vmlab template push` publishes a template to any OCI
  registry, and `pull` installs it from one.

A lab's `template =` may also name a registry reference directly, with an
explicit `arch` on the VM. `vmlab up` pulls it when it is absent from the store
and never re-pulls it implicitly. Updates are explicit, through `vmlab pull` or
`vmlab template pull`.

## template {}

One buildable template. The build creates a working disk from the `source`,
boots it with the hardware declared here, runs the provisions and playbooks,
shuts the VM down and seals the disk into the store under
`<arch>/<name>/<version>/`. The hardware fields are recorded in the template's
metadata and become the inheritance layer for every VM cloned from it.

Template blocks live at the top level of a `vmlab.wcl`, beside a `lab {}` or in
a file of their own with no lab at all. `vmlab template build` reads
`./vmlab.wcl` unless `--file` names another.

```wcl
template "<name>" {
  arch        = "x86_64"
  version     = "24.04"
  registry    = "ghcr.io/owner/ubuntu-24.04"
  profile     = "linux-modern"
  cpus        = 2
  memory      = 4GiB
  disk        = 20GiB
  display     = "virtio-vga"
  firmware    = "ovmf"
  tpm         = false
  secure_boot = false
  nested      = false
  gui         = false
  qemu_args   = []
  first_boot  = "scripts/first-boot.ws"
  agent       = true
  source "…"  { … }
  media       { … }
  provision "…" { … }
  playbook "…"  { … }
  nic         { … }
  disk "…"    { … }
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | utf8 (label) | required | Template name, for example `linux-modern`; the inline block label. |
| `arch` | utf8 | required | Architecture. Selects the QEMU system emulator. |
| `version` | utf8 | required | Version string, non-empty. Name, arch and version together are unique. |
| `registry` | utf8 | none | Full OCI repository to publish to and to version-bump against. |
| `profile` | utf8 | none | Guest OS profile supplying hardware defaults for the build VM. |
| `cpus` | i64 | from profile | vCPU count for the build VM. Inherited by clones. |
| `memory` | ByteSize | from profile | RAM for the build VM, for example `8GiB`. Inherited by clones. |
| `disk` | ByteSize | from source | Working disk size for the build, for example `64GiB`. Required for a `scratch` source. |
| `display` | utf8 | from profile | QEMU display device string for the build VM. |
| `firmware` | utf8 | from profile | Firmware: `ovmf` or `seabios`. |
| `tpm` | bool | from profile | Enable a TPM 2.0 device. |
| `secure_boot` | bool | from profile | Enable secure boot; OVMF only. |
| `nested` | bool | `false` | Enable nested virtualisation for the build VM. |
| `gui` | bool | `false` | Watch the build VM in a VNC viewer. |
| `qemu_args` | list<utf8> | none | Raw QEMU flags for the build VM. The escape hatch. |
| `first_boot` | utf8 | none | wscript run on the first instantiation of a clone, before it turns ready. |
| `agent` | bool | `true` | Bake the vmlab-agent service into the image. |
| `source {}` | child | required | What the build starts from. Exactly one of four forms. |
| `media {}` | children | none | ISO or floppy images attached to the build. |
| `provision {}` | children | none | Provision scripts that drive the build. |
| `playbook {}` | children | none | config-weave playbooks applied to the build VM, interleaved with provisions in declaration order. Steps stream as structured build progress. |
| `nic {}` | children | none | NICs for the build VM. Optional; the build VM may be air-gapped. |
| `disk {}` | children | none | Additional disks attached during the build. |

`media`, `provision`, `playbook`, `nic` and `disk` are the same blocks a `vm`
carries; see vm.md.

### Versions and the registry

The declared `version` is a fixed prefix naming the upstream identity, such as
an OS release. Each build appends a counter: the stored version is
`<version>.<N>`, where N is one higher than the highest existing build with that
prefix, or 0 for the first. When `registry` is set the existing builds are read
from its tags, so the counter continues across machines; otherwise the local
store decides. Changing the prefix restarts the counter at `.0`. `vmlab template
build --version` pins an explicit version instead. `registry` is also the
repository `vmlab template push` publishes to.

### The first-boot script and the agent

`first_boot` is read at build time and baked into the template's metadata, so a
clone runs it on its first start without the lab needing the file. It runs
before the VM turns ready. With `agent = true` the build waits for the
guest-installed agent's handshake before sealing and records the agent version in
the metadata. With `agent = false`, or on a profile with no agent channel, the
check is skipped and the build log says so.

### Validation

- `arch` is one of the known architectures: `x86_64`, `x86`, `aarch64`,
  `riscv64`, `loongarch64`, `s390x`, `ppc64`.
- `version` is non-empty, and no other template in the file has the same arch,
  name and version.
- `profile`, if set, names a known profile.
- A `scratch` source requires `disk`.
- `first_boot` and every `provision` script exist under the template's root and
  compile.
- A `playbook` on a template whose `arch` is not `x86_64` is rejected.
- Every `media` and `disk` source folder exists.

## source {}

What the build starts from. The inline label is the kind, and the kind decides
which fields apply. Exactly one `source {}` per template; the schema rejects a
template without one.

```wcl
source "iso"      { path = "./installer.iso" }
source "iso"      { url = "https://…/installer.iso" sha256 = "…" }
source "qcow2"    { path = "./base.qcow2" }
source "template" { from = "x86_64/ubuntu-24.04@24.04.4" }
source "scratch"  { }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `kind` | utf8 (label) | required | Source kind: `iso`, `qcow2`, `template` or `scratch`; the inline label. |
| `path` | utf8 | none | Local file path, for `iso` and `qcow2`. Mutually exclusive with `url`. |
| `url` | utf8 | none | Remote artefact URL, for `iso` and `qcow2`. Requires `sha256`. |
| `sha256` | utf8 | none | SHA-256 of the remote artefact. Required with `url`. |
| `from` | utf8 | none | Source template `<arch>/<name>[@<version>]`, for kind `template`: a layered build. |

The four kinds:

- **`iso`** — an installer ISO. The build boots it with the attached media and
  lets the provision script drive the installer. A remote artefact is downloaded
  to the artefact cache and verified against `sha256` before use.
- **`qcow2`** — an existing disk image, by path or URL and hash, imported as the
  base.
- **`template`** — a layered build. An existing store template named by `from` is
  the base: the build starts from that template's disk and its recorded
  hardware, runs more provisioning, and seals a new template. `from` must be a
  local store reference; a registry reference or `scratch` is rejected here.
- **`scratch`** — a blank disk of the template's declared `disk` size. The
  attached installer media and provision script do everything.

Validation requires exactly one of `path` and `url` for `iso` and `qcow2`,
`sha256` whenever `url` is set, a `path` file that exists, and a `from` template
that is in the store. An unknown kind is rejected.

## OCI registries

The online way to share a template is an OCI registry: GHCR, Docker Hub, Harbor,
or any self-hosted registry that speaks the OCI distribution API.

### An artifact, not an image

A pushed template is an OCI manifest whose `artifactType` is a vmlab-specific
string, whose config blob is the template's metadata, and whose layers are the
disk in pieces, each with a vmlab media type. A `docker pull` of a vmlab
reference fails fast as "not a container image" rather than half-working, and
`vmlab template pull` refuses a manifest whose artifact type is not vmlab's
rather than installing a container's layers as a disk. The media type strings
are frozen: they are part of the on-the-wire contract and never change.

### Chunking

The qcow2 is split into fixed-size chunks, 512 MiB by default, each compressed
with zstd and pushed as one ordered layer blob. The manifest's annotations record
the chunk count, the chunk size, the total size and the digest of the assembled,
uncompressed image. A pull downloads the chunks, reassembles them in order and
verifies the whole-image digest before installing to the store. Everything
streams through bounded buffers, so a 64 GiB image never lands in memory.

The size is chosen for GHCR, which enforces a per-layer size limit and a
per-upload timeout. The timeout is the binding constraint on realistic upstream
bandwidth, and 512 MiB clears it with a wide margin while keeping retries cheap.
Change it with `oci_chunk_size` in the host configuration (host-profiles.md).

### Addressing

A registry reference is `registry/owner/name[:tag]`, and the registry host is
always explicit. vmlab treats the first path segment as a host only when it looks
like one: it contains a dot, a colon, or is exactly `localhost`. A bare
`owner/name` is rejected with a message asking for a registry, so nothing ever
reaches Docker Hub by accident.

The tag is the template version. A push of `x86_64/ubuntu-24.04@24.04.4.1` to
`ghcr.io/owner/ubuntu-24.04` lands at the tag `24.04.4.1`, and a pull of that
reference installs the template into the store as
`x86_64/ubuntu-24.04@24.04.4.1`, with the originating reference recorded in its
metadata. The store name is the last path component of the repository.

### Moving tags and prefixes

Every push also re-points a moving alias. By default that is `latest`; with
`--prerelease` it is `latest-prerelease` instead, so a pre-release never
displaces the stable pointer. When a lab references a registry template by a
moving alias, or by a build-counter prefix such as `26100.1742`, vmlab resolves
it against the registry's published tags to a concrete version before pulling. A
concrete version already in the store is used offline without contacting the
registry at all.

### Multi-arch

A tag may resolve through an OCI image index keyed by platform architecture. This
maps the store's arch dimension onto OCI's own multi-platform mechanism: a push
of the `aarch64` build of a template adds a platform entry to the index beside
the `x86_64` one. Consistent with the store, the arch is always explicit. `vmlab
template pull` requires `--arch` when the tag resolves to an index with more than
one platform, and never silently assumes the host's.

### Push and pull

`vmlab template push <arch>/<name>[@<version>] [target]` publishes a store entry.
The target defaults to the `registry` field the template block declared, which
the metadata records, so a template built for a known home needs no argument.
`--source` links the package to a source repository URL, and defaults to the
`origin` remote of the directory you run it in when that resolves to a web URL.
The push is performed by the supervisor, streams progress to your terminal, and
can be stopped from another terminal with `vmlab template stop`.

`vmlab template pull <registry/owner/name:version>` installs into the store and
refuses to replace a version already there unless you pass `--overwrite`. A lab
file can skip the explicit pull: a `vm` whose `template` is a registry reference,
with an `arch` beside it, is pulled by the supervisor before the lab daemon
starts, with progress events you can watch, and never re-pulled once present.

Updates are explicit. A template present in the store is never re-pulled
implicitly, even when the registry has a newer build under the same moving tag.
Run `vmlab pull` in the lab, or `vmlab template pull`, to fetch a newer version.
Existing clones keep backing onto the version they were created from.

### Credentials

vmlab reuses the Docker credential configuration already on the machine, so a
`ghcr.io` login made for `docker` works. It reads `~/.docker/config.json`, or
`$DOCKER_CONFIG/config.json`, and honours both the inline `auths` entries and the
`credHelpers` and `credsStore` fields, invoking the named
`docker-credential-<helper>` binary the way Docker does. Registries that answer
with a Bearer challenge get the standard token flow. A missing credential is
never fatal: anonymous pulls of public templates must work, so it simply means an
anonymous request.

For a machine with no Docker tooling:

```sh
vmlab template login <registry> --username <user> --password <secret>
```

It validates the credential against the registry and stores it in the same Docker
config file, so a later push or pull finds it.

### Finding templates

`vmlab template search` lists the templates published under a registry namespace,
filtered by a name substring, an arch, and whether you want VM templates or
container images. The namespaces it searches are host-level settings managed with
`vmlab template registry add | list | remove`. They are search roots, not
secrets: credentials stay in the Docker config and are looked up by registry
host.
