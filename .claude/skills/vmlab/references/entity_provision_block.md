# provision {} block

_wcl block_

Names a wscript file that runs on `vmlab up`; scoping it to VMs gates their depends_on.

A `provision {}` block runs a wscript file on `vmlab up`, in declaration order. Targeting is explicit: name the VMs in `vms`, or take the whole-lab opt-in with `lab_wide = true`.

```wcl
provision "scripts/setup.ws" { lab_wide = true }     // runs once, after every machine is up
provision "scripts/join.ws"  { vms = ["client01"] }  // scoped: gates depends_on on these VMs
provision "scripts/todo.ws"  { }                     // neither: declared, never runs
```

**Provision failures fail `vmlab up`.** A scoped provision (`vms = [...]`) gates `depends_on` on those VMs: dependents wait for the provision to finish. `vms` and `lab_wide` are mutually exclusive; a block with neither is declared but skipped (`up` says so), which is the state a half-written provision sits in. Inside a `template {}` neither applies — build provisions always run on the build VM.

## Related

- [Provisions & event handlers](../references/concept_provisions.md)

- [on "event" {} handler](../references/entity_on_handler.md)

- [wscript: overview](../references/concept_wscript_overview.md)

- [lab {} block](../references/entity_labs.md)

[← Back to SKILL.md](../SKILL.md)
