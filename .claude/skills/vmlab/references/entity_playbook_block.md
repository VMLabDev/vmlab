# playbook {} block

_wcl block_

Binds a config-weave playbook folder to lab machines (or a template build): the play to run and which VMs/containers it targets.

A `playbook {}` block is valid inside `lab {}` and inside `template {}`. The
inline label is the playbook folder (containing `playbook.wcl`), relative to
the lab root; `play` names the play inside it (required), and `vms` scopes the
target machines — VM and container names in one namespace, empty or absent
meaning every machine.


```wcl
lab "ad-demo" {
  vm "dc01"  { template = "x86_64/windows-2025" nic { segment = "corp" } }
  vm "app01" { template = "x86_64/windows-2025" nic { segment = "corp" } }

  provision "scripts/prep.ws"  { vms = ["dc01"] }        // runs first…
  playbook "playbooks/domain" { play = "dc"     vms = ["dc01"] }   // …then this
  playbook "playbooks/domain" { play = "member" vms = ["app01"] }
}
```

On `vmlab up`, playbooks and provisions apply \*\*interleaved in declaration
order\*\* — a provision after a playbook sees the converged guest, and
`depends_on` waves gate on both kinds. The same block inside a `template {}`
applies to the build VM, its steps streaming as structured build progress.
Re-run any declaration later with `vmlab playbook check|apply <machine>`, or
from the machine's **Playbook** tab in the web console
([concept](../references/concept_playbooks.md)).


## Related

- [Playbooks (config-weave)](../references/concept_playbooks.md)

- [lab {} block](../references/entity_labs.md)

- [template {} block](../references/entity_template_block.md)

- [provision {} block](../references/entity_provision_block.md)

- [The vmlab.wcl schema](../references/fact_schema_reference.md)

[← Back to SKILL.md](../SKILL.md)
