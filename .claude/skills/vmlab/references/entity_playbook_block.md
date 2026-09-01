# playbook {} block

_wcl block_

Binds a config-weave play to the machine that declares it, with the variables for that machine's run.

A `playbook {}` block is valid inside `vm {}`, `container {}`, and
`template {}`. The inline label is the playbook folder (containing
`playbook.wcl`), relative to the lab root, and `play` names the play inside it
(required). The machine that declares the block is the machine the play
converges — there is no target list. Inside a `template {}` the block applies to
the build VM, its steps streaming as structured build progress.


```wcl
lab "ad-demo" {
  vm "dc01" {
    template = "x86_64/windows-2025"
    nic { segment = "corp" }

    provision "scripts/prep.ws" { }             // runs first…
    playbook "playbooks/domain" {               // …then this
      play = "dc"
      var "domain" { value = "corp.example.com" }
    }
  }

  vm "app01" {
    template = "x86_64/windows-2025"
    nic { segment = "corp" }

    playbook "playbooks/domain" {
      play = "member"
      var "domain"      { value = "corp.example.com" }
      var "member_name" { value = "APP01" }     // this machine only
    }
  }
}
```

Each `var "<name>" { value = "…" }` child becomes one `--var name=value` on the
config-weave command line, in declaration order — so one play converges several
machines with different settings instead of hardcoding them in `playbook.wcl`.
Values pass through **verbatim**, and config-weave reads each as a WCL
expression where it can (`3` is an int, `true` a bool) and as a string
otherwise; quote a value (`"3"`) to force text. Names must be WCL identifiers
and cannot repeat within one block — `vmlab validate` catches both. Overrides
beat the playbook's own `vars {}` defaults.


On `vmlab up`, a machine's playbooks and provisions apply \*\*interleaved in
declaration order\*\* — a provision after a playbook sees the converged guest —
and `depends_on` waves gate on the whole set. Re-run any declaration later with
`vmlab playbook check|apply <machine>` ([concept](../references/concept_playbooks.md)).


## Related

- [Playbooks (config-weave)](../references/concept_playbooks.md)

- [lab {} block](../references/entity_labs.md)

- [template {} block](../references/entity_template_block.md)

- [provision {} block](../references/entity_provision_block.md)

- [The vmlab.wcl schema](../references/fact_schema_reference.md)

[← Back to SKILL.md](../SKILL.md)
