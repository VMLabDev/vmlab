# What `vmlab validate` checks

`vmlab validate` performs schema + semantic checks with no side effects. It verifies:

| Check | Detail |
| --- | --- |
| WCL schema | The file conforms to the vmlab schema |
| Template refs | Exist in the store, or a registry ref is given with explicit `arch` |
| NIC segments | Every NIC's segment is declared |
| Static IPs | Inside the declared subnet; no duplicate static IPs or MACs |
| Dependencies | No `depends_on` cycles |
| Scripts | Provision/handler files exist AND compile (full wscript type-check) |
| Scratch VMs | Have `arch` + `profile` + `disk` |
| Secure boot | `secure_boot = true` needs UEFI: rejected when firmware resolves to SeaBIOS (or, on x86, to nothing — SeaBIOS is QEMU's default). Resolved VM > template > profile, and the error names the layer each value came from |
| Events | Every `on` event name is known, and `targets` is only declared on machine-scoped events (`vm.*` / `container.*` / `snapshot.*`) |
| Playbooks | The folder and its `playbook.wcl` exist; `var` names are WCL identifiers, and no name is declared twice within one `playbook` block |
| Shares | A machine declaring an SMB-capable share (`transport` `smb` or the default `auto`) has a NIC on a segment |
| Logins | On a Windows-family profile every `login` has a `password`; on a Linux-family one none declares `elevated`; labels are unique per machine and at most one sets `default = true` (§19.2). A profile that names no family (`custom`, or a registry template's) triggers neither family rule |

## Related

- [lab {} block](../references/entity_labs.md)

- [vm {} block](../references/entity_vms.md)

- [Provisions & event handlers](../references/concept_provisions.md)

- [The vmlab.wcl schema](../references/fact_schema_reference.md)

[← Back to SKILL.md](../SKILL.md)
