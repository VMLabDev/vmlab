# media {} block

_wcl block_

Turns a host folder into an ISO or floppy image with a content-addressed cache; declarative, no CLI.

A `media {}` block turns a host folder into an ISO or floppy image, with a
content-addressed cache (rebuilt only when the folder changes). It is declared
inline in VM or template definitions — there is no `vmlab media` CLI.


```wcl
media { kind = "iso"    from = "./unattend/" label = "CIDATA" }
media { kind = "floppy" from = "./drivers/"  label = "DRV" }
```

## Related

- [vm {} block](../references/entity_vms.md)

- [Templates](../references/concept_templates.md)

- [template {} block](../references/entity_template_block.md)

- [Scratch VMs](../references/concept_scratch_vms.md)

[← Back to SKILL.md](../SKILL.md)
