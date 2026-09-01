# provision {} block

_wcl block_

Names a wscript file that runs on `vmlab up`, declared inside the vm/container it configures.

A `provision {}` block runs a wscript file on `vmlab up`. It is declared **inside** the `vm {}` or `container {}` it belongs to — that machine is its target, so there is nothing to name. It runs once, after that machine is ready, at its position among the machine's steps.

```wcl
vm "dc01" {
  template = "x86_64/windows-server-2025"

  provision "scripts/setup.ws" { }              // 1st, once dc01 is ready
  playbook  "playbooks/domain" { play = "dc" }  // 2nd
  provision "scripts/verify.ws" { }             // 3rd, sees the converged guest
}
```

**Provision failures fail `vmlab up`.** A machine's steps gate `depends_on`: anything depending on `dc01` starts only after dc01's provisions and playbooks have finished. Inside the script, `lab.this_vm()` returns the owning machine — VM or container alike — while `lab.machine("name")` reaches any other — a script that stands up a DC and then joins a member is still one script, declared on whichever machine should gate it. The same block inside a `template {}` runs on the build VM.

## Related

- [Provisions & event handlers](../references/concept_provisions.md)

- [on "event" {} handler](../references/entity_on_handler.md)

- [wscript: overview](../references/concept_wscript_overview.md)

- [lab {} block](../references/entity_labs.md)

- [playbook {} block](../references/entity_playbook_block.md)

[← Back to SKILL.md](../SKILL.md)
