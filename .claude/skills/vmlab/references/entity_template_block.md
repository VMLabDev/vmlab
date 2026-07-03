# template {} block

_wcl block_

Declares a disk template to build: arch, version, profile, working disk, exactly one source, build media/NICs and provisions.

A `template {}` block declares a disk image to build. It lives in `vmlab.wcl` (or any
file passed with `-f`) and is realised by [a build](../references/concept_template_builds.md).


```wcl
import <vmlab.wcl>

template "linux-modern" {
  arch    = "x86_64"           // required — selects the QEMU system emulator
  version = "1.0"              // required
  profile = "linux-modern"     // hardware defaults
  cpus    = 4                  // optional hardware overrides (also memory, display,
  memory  = 8GiB               // firmware, tpm, secure_boot, nested, qemu_args)
  disk    = 20GiB              // working disk size for the build
  gui     = true               // watch the build VM's screen in a QEMU window

  source "iso" { url = "https://releases.ubuntu.com/.../x.iso" sha256 = "abc123..." }

  media { kind = "iso" from = "./cloudinit/" label = "CIDATA" }   // built from folder, cached
  nic { nat = true }                                              // build VM network access
  disk "extra" { size = 10GiB }                                   // extra disks during build
  provision "scripts/install.ws" { }                         // drives the installer
}
```

Exactly one [`source {}` block](../references/entity_template_sources.md) selects what the build starts from.

## Related

- [Templates](../references/concept_templates.md)

- [Template build flow](../references/concept_template_builds.md)

- [source {} build source](../references/entity_template_sources.md)

- [media {} block](../references/entity_media.md)

- [Guest OS profiles](../references/concept_profiles.md)

- [The vmlab.wcl schema](../references/fact_schema_reference.md)

[← Back to SKILL.md](../SKILL.md)
