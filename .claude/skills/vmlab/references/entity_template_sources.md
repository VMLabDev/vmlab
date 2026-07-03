# source {} build source

_wcl block_

Exactly one source {} block per template selects what the build starts from: iso, qcow2, template, or scratch.

A template has exactly one `source {}` block, selecting what the build starts from:

```wcl
source "iso"      { path = "./isos/win11.iso" }            // local installer ISO
source "iso"      { url = "https://..." sha256 = "..." }   // downloaded + verified (sha256 required)
source "qcow2"    { path = "./base.qcow2" }                // existing disk image as base (url+sha256 OK)
source "template" { from = "x86_64/linux-modern@1.0" }     // layered build from a stored template
source "scratch"  { }                                       // blank disk; installer media does everything
```

## Related

- [template {} block](../references/entity_template_block.md)

- [Templates](../references/concept_templates.md)

- [Template build flow](../references/concept_template_builds.md)

[← Back to SKILL.md](../SKILL.md)
