# Template build flow

_A build resolves the source, boots a synthesised one-VM lab, runs the provisions, then flattens and seals into the store; a failed build leaves nothing behind._

`vmlab template build` resolves the source (URL downloads are cached and
content-addressed under `~/.cache/vmlab/artefacts/`), creates a working qcow2,
synthesises a one-VM lab from the template definition, boots it per the hardware
profile, runs the provision scripts (full wscript API — keystrokes, screen matching,
exec; the script should install the QEMU guest agent), shuts down gracefully,
flattens and seals into the store. **A failed build leaves nothing behind.** The
sealed result is `~/.local/share/vmlab/templates/<arch>/<name>/<version>/`
(`disk.qcow2` + `template.wcl`). The step-by-step runbook is
[Build a disk template](../references/process_build_template.md).


Once QGA answers, the build also installs **vmlab-agent** into the image
automatically (before provisions run) and records its version in the sealed
meta as `agent_version` — that is what enables `vmlab shell`, agent file
transfer, `tail`, `eventlog` and wscript `terminal()`/`stats()` on VMs cloned
from the template. Templates built before the agent existed keep working with
QGA-only exec/copy.


## Related

- [Templates](../references/concept_templates.md)

- [template {} block](../references/entity_template_block.md)

- [source {} build source](../references/entity_template_sources.md)

- [Build a disk template](../references/process_build_template.md)

- [media {} block](../references/entity_media.md)

[← Back to SKILL.md](../SKILL.md)
