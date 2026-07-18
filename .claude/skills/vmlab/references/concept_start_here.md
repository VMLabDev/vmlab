# Start here

_Getting started using VMLab._

VMLab is a system where you describe the contents of how you want your lab environment setup
in a `vmlab.wcl` file and you can recreate it by running a single command.


```wcl
lab "first" {
  segment "corp" { }
  vm "web" {
    template = "ghcr.io/vmlabdev/vmlab-templates/alpine-3.23"
    nic { segment = "corp" }
  }
}
```

```console
vmlab up                        # provision and start lab environment
vmlab status                    # state, IPs, ready flags
vmlab shell web                 # root shell over the agent channel
vmlab down                      # graceful stop; clones retained
```

## Related

- [vmlab.wcl](../references/entity_vmlab_wcl.md)

- [Bring a lab up and tear it down](../references/process_golden_path.md)

- [How vmlab fits together](../references/concept_architecture.md)

- [Templates](../references/concept_templates.md)

- [wscript: overview](../references/concept_wscript_overview.md)

- [The web console](../references/concept_web_console.md)

[← Back to SKILL.md](../SKILL.md)
