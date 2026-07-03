# provision {} block

_wcl block_

Names a wscript file that runs on `vmlab up`; scoping it to VMs gates their depends_on.

A `provision {}` block runs a wscript file on `vmlab up`, in declaration order.

```wcl
provision "scripts/setup.ws" { }                     // runs on `vmlab up`, in order
provision "scripts/join.ws"  { vms = ["client01"] }  // scoped: gates depends_on on these VMs
```

**Provision failures fail `vmlab up`.** A scoped provision (`vms = [...]`) gates `depends_on` on those VMs: dependents wait for the provision to finish.

## Related

- [Provisions & event handlers](../references/concept_provisions.md)

- [on "event" {} handler](../references/entity_on_handler.md)

- [wscript: overview](../references/concept_wscript_overview.md)

- [lab {} block](../references/entity_labs.md)

[← Back to SKILL.md](../SKILL.md)
