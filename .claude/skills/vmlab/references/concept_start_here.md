# Start here

_Orientation: the three artifacts you work with (vmlab.wcl, templates, wscript), your first lab, and where to go next._

vmlab builds disposable virtual machine labs on a single host. You describe a
lab — its machines and the virtual networks connecting them — in one file, and
vmlab makes it real: linked-clone disks, DHCP/DNS, isolated segments, guest
automation. Everything runs unprivileged on QEMU/KVM; the only host grant a
baseline lab needs is `/dev/kvm`.

Day to day you touch exactly three kinds of artifact:

1. **[`vmlab.wcl`](../references/entity_vmlab_wcl.md)** — the lab definition, written in WCL.
   One per lab directory; every `vmlab` command finds it by walking up from
   the current directory.
2. **[Templates](../references/concept_templates.md)** — sealed, reusable disk images
   (`x86_64/linux-modern@1.2`) that VMs clone from. Build them yourself or
   pull them from an OCI registry.
3. **[wscript](../references/concept_wscript_overview.md)** — the scripting language for guest
   automation: provision scripts, event handlers, ad-hoc drives.


```wcl
lab "first" {
  segment "corp" { }
  vm "web" {
    template = "x86_64/linux-modern"
    nic { segment = "corp" }
  }
}
```

```console
vmlab validate && vmlab up      # clone, boot, provision
vmlab status                    # state, IPs, ready flags
vmlab shell web                 # root shell over the agent channel
vmlab down                      # graceful stop; clones retained
```

From here: walk [the golden path](../references/process_golden_path.md) for the full everyday
loop, read [how vmlab fits together](../references/concept_architecture.md) for the moving
parts, or skip straight to the part covering your task — labs & networking,
templates, containers, automation, or the [web console](../references/concept_web_console.md)
if you'd rather drive everything from a browser.


## Related

- [vmlab.wcl](../references/entity_vmlab_wcl.md)

- [Bring a lab up and tear it down](../references/process_golden_path.md)

- [How vmlab fits together](../references/concept_architecture.md)

- [Templates](../references/concept_templates.md)

- [wscript: overview](../references/concept_wscript_overview.md)

- [The web console](../references/concept_web_console.md)

[← Back to SKILL.md](../SKILL.md)
