# Shipped guest OS profiles

Hardware defaults selected with `profile = "..."`. Precedence is **VM block > template metadata > profile > defaults**, resolved in one place for both machine kinds; a `container {}` names a profile too, and has no template layer. Override or extend by dropping `*.wcl` into `~/.config/vmlab/profiles/`.

| Profile | Machine | Firmware | Secure boot | TPM | Disk bus | NIC | Display | CPUs/Mem |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `windows-11` | q35 | ovmf | yes | yes | virtio | virtio-net-pci | qxl | 4 / 8G |
| `windows-server` | q35 | ovmf | no | yes | virtio | virtio-net-pci | qxl | 4 / 8G |
| `windows-legacy` | pc | seabios | no | no | ide | e1000 | std | 2 / 2G |
| `linux-modern` | q35 | ovmf | no | no | virtio | virtio-net-pci | virtio-gpu | 2 / 4G |
| `linux-generic` | q35 | seabios | no | no | virtio | virtio-net-pci | std | 2 / 2G |
| `container` | — | — | — | — | — | — | — | 1 / 256M — the micro-VM floor for a `container {}` (§18); nothing else applies to one |
| `custom` | — | — | — | — | — | — | — | nothing assumed; supply everything via attributes + `qemu_args` |

## Related

- [Guest OS profiles](../references/concept_profiles.md)

- [vm {} block](../references/entity_vms.md)

- [Templates](../references/concept_templates.md)

[← Back to SKILL.md](../SKILL.md)
