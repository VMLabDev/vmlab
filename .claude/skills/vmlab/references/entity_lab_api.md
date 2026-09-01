# Lab

_api object_

The lab handle passed to fn main(lab: Lab) / fn handle(event, lab) — find machines and segments, log.

| Method | Returns | Notes |
| --- | --- | --- |
| `lab.name()` | `string` | Lab name from vmlab.wcl |
| `lab.log(msg: string)` | `unit` | Lab log + live CLI stream |
| `lab.machine(name: string)` | `Result[Machine, string]` | Any machine, whichever kind. Err if not defined |
| `lab.machines()` | `List[Machine]` | Every machine in the lab |
| `lab.vm(name: string)` | `Result[Machine, string]` | Err if not defined, or if the name is a container |
| `lab.this_vm()` | `Result[Machine, string]` | The machine this script is declared on — the normal way a nested `provision {}` names its own guest, VM or container alike. Err outside a machine's own provision or a template first-boot script |
| `lab.vms()` | `List[Machine]` | The VMs only |
| `lab.container(name: string)` | `Result[Machine, string]` | Err if not defined, or if the name is a VM |
| `lab.containers()` | `List[Machine]` | The containers only |
| `lab.segment(name: string)` | `Result[Segment, string]` | Err if not declared |

There is one machine handle, not one per kind. `lab.vm` and `lab.container` are
`lab.machine` with a kind check, kept because they read well and because
"that's a container" is a better error than "no such machine" — but all five
return the same [Machine](../references/entity_vm_api.md).


## Free functions

| Function | Notes |
| --- | --- |
| `vmlab::sleep_ms(ms: int)` | Sleep; call module-qualified (or `use vmlab::sleep_ms`). Prefer `wait_*` methods over fixed sleeps. |

## Related

- [Machine](../references/entity_vm_api.md)

- [Container handle](../references/entity_container_api.md)

- [Segment](../references/entity_seg_api.md)

- [wscript: pattern matching & errors](../references/concept_wscript_matching.md)

- [Provisions & event handlers](../references/concept_provisions.md)

[← Back to SKILL.md](../SKILL.md)
