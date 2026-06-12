# Template builds reference

Templates are sealed qcow2 images in the local store, referenced by
`<arch>/<name>[@<version>]` (omitting the version means latest). Labs boot
linked clones backed by them — templates are never written to by labs.

## Template definition (WCL)

`template {}` blocks live in `vmlab.wcl` (or any WCL file passed with
`-f`), alongside or instead of a `lab {}`:

```wcl
import <vmlab.wcl>

template "linux-modern" {
  arch    = "x86_64"           // required — selects the QEMU system emulator
  version = "1.0"              // required
  profile = "linux-modern"     // hardware defaults (see host-config.md)
  cpus    = 4                  // optional hardware overrides (also memory,
  memory  = "8G"               // display, firmware, tpm, secure_boot, nested,
  disk    = "20G"              // qemu_args). disk = working disk size for the build
  gui     = true               // watch the build VM's screen in a QEMU window

  source "iso" { url = "https://releases.ubuntu.com/.../x.iso" sha256 = "abc123..." }

  media { kind = "iso" from = "./cloudinit/" label = "CIDATA" }   // built from folder, cached
  nic { nat = true }                                              // build VM network access
  disk "extra" { size = "10G" }                                   // extra disks during build
  provision "scripts/install.wisp" { }                            // drives the installer
}
```

## Build sources (exactly one `source` block)

```wcl
source "iso"      { path = "./isos/win11.iso" }                       // local installer ISO
source "iso"      { url = "https://..." sha256 = "..." }              // downloaded + verified (sha256 required)
source "qcow2"    { path = "./base.qcow2" }                           // existing disk image as base (url+sha256 also OK)
source "template" { from = "x86_64/linux-modern@1.0" }                // layered build from a stored template
source "scratch"  { }                                                  // blank disk; installer media does everything
```

## Build flow

`vmlab template build` → resolve source (URL downloads cached +
content-addressed under `~/.cache/vmlab/artefacts/`) → create working qcow2
→ synthesize a one-VM lab from the template definition → boot per the
hardware profile → run provision scripts (full wisp API: keystrokes, screen
matching, exec — see wisp-api.md; the script should install the QEMU guest
agent) → graceful shutdown → flatten → seal into the store. A failed build
leaves nothing behind.

Store layout: `~/.local/share/vmlab/templates/<arch>/<name>/<version>/`
containing `disk.qcow2` + `template.wcl` (metadata: hardware, profile,
origin, sha256).

## CLI

```sh
vmlab template build                       # all template {} blocks in ./vmlab.wcl
vmlab template build -f templates.wcl base # one named template from a specific file
vmlab template list
vmlab template rm x86_64/base@1.0 [--force]   # exact version; --force if clones depend on it
vmlab template export x86_64/base@1.0 base.tar.zst
vmlab template import base.tar.zst [--overwrite]
```

For registry distribution (`push`/`pull`/`login`) see `oci.md`.

## Scratch VMs in labs

A lab VM can skip templates entirely: `template = "scratch"` boots a blank
disk. Requires explicit `arch`, `profile`, and `disk` size; boot media is
your problem (typically `cdrom` + a `media` block). Scratch never appears
in the store and cannot be pushed/pulled.

## Standalone media building

Folder → ISO/floppy with a content-addressed cache (rebuilt only when the
folder changes). Declarative `media {}` blocks in VM/template definitions
do the same thing automatically.

```sh
vmlab media build iso ./drivers/ out.iso -l DRIVERS
vmlab media build floppy ./unattend/ unattend.img
```

Source of truth: PRD §6; `src/config/schema.wcl` (TemplateDef,
TemplateSource), `src/template/build.rs`, `src/template/cli.rs`.
